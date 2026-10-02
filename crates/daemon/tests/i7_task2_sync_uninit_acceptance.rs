//! #57 (I7 task 2): `sync` + `uninit` over the real local IPC and the
//! real CLI.
//!
//! "The watcher missed it" is an inert watch source: attached, claiming
//! continuity, delivering nothing -- so only an explicit sync can see a
//! change. No semantic backend is configured in these homes, so every
//! sync here is also the "backend missing" case.

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
    time::Duration,
};

use brainprint_core::{
    PROTOCOL_VERSION, WorkspaceId,
    protocol::{
        self, ClientConnection, ErrorKind, ErrorResponse, HandshakeRequest, InitRequest,
        InitResponse, Request, Response,
        maintenance::{
            DoctorRequest, DoctorWorkspaceWire, RebuildRequest, SyncRequest, SyncResponse,
            UninitRequest, UninitResponse, WatcherCheckWire,
        },
        query::*,
        work::{PostCommandRefreshWire, ResourceDeltaKindWire},
    },
};
use brainprint_daemon::{
    query::{DaemonQueryRuntime, lifecycle::WatchFactory},
    runtime_paths,
    server::Server,
};
use brainprint_engine::{
    knowledge::{
        NewPolicy, NewWorkItem, PriorityClass, ProjectKnowledgeStore, ProtectionClass, Provenance,
        SourceKind, WorkItemSourceKind, WorkRuntime,
    },
    paths::{GlobalPaths, WorkspacePaths},
    watch::{RawWatchEvent, WatchSource},
};
use rusqlite::{Connection, OpenFlags};

// ------------------------------------------------------------- harness

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let path = env::temp_dir().join(format!(
            "bp-i7t2-{label}-{}-{}",
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

fn write_sources(root: &Path) {
    let src = root.join("src");
    fs::create_dir_all(&src).expect("src");
    fs::write(src.join("shared.ts"), SHARED_TS).expect("shared.ts");
    fs::write(src.join("app.ts"), APP_TS).expect("app.ts");
}

fn fixture_workspace(label: &str) -> TestDir {
    let workspace = TestDir::create(label);
    write_sources(workspace.path());
    workspace
}

/// Attached and continuous, but it never reports anything.
struct DeafWatch;

impl WatchSource for DeafWatch {
    fn drain(&mut self) -> Vec<RawWatchEvent> {
        Vec::new()
    }
}

fn deaf() -> WatchFactory {
    Arc::new(|_root: &Path| Ok(Box::new(DeafWatch) as Box<dyn WatchSource>))
}

struct Daemon {
    handle: tokio::task::JoinHandle<()>,
    endpoint: runtime_paths::RuntimeEndpoint,
    runtime: Arc<DaemonQueryRuntime>,
}

impl Daemon {
    /// `None`: the real platform watcher.
    async fn start(global: &GlobalPaths, watch: Option<WatchFactory>) -> Self {
        let mut server = match watch {
            Some(factory) => Server::bind_with_watch_factory(global, factory).await,
            None => Server::bind(global).await,
        }
        .expect("bind");
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
                client_kind: "i7-task2".to_owned(),
            }),
        )
        .await;
        roundtrip(&mut connection, request).await
    }

    async fn init(&self, root: &Path) -> InitResponse {
        match self
            .send(Request::Init(InitRequest { path: path(root) }))
            .await
        {
            Response::Init(response) => response,
            other => panic!("init failed: {other:?}"),
        }
    }

    async fn init_id(&self, root: &Path) -> WorkspaceId {
        self.init(root).await.workspace_id.parse().expect("id")
    }

    async fn sync(&self, root: &Path) -> Result<SyncResponse, ErrorResponse> {
        match self
            .send(Request::Sync(SyncRequest { path: path(root) }))
            .await
        {
            Response::Sync(response) => Ok(response),
            Response::Error(error) => Err(error),
            other => panic!("unexpected sync response: {other:?}"),
        }
    }

    async fn uninit(&self, root: &Path) -> Result<UninitResponse, ErrorResponse> {
        match self
            .send(Request::Uninit(UninitRequest { path: path(root) }))
            .await
        {
            Response::Uninit(response) => Ok(response),
            Response::Error(error) => Err(error),
            other => panic!("unexpected uninit response: {other:?}"),
        }
    }

    async fn query_outcome(
        &self,
        workspace: WorkspaceSelectorWire,
        operation: QueryOperationWire,
    ) -> QueryOutcomeWire {
        let Response::Query(response) = self
            .send(Request::Query(QueryRequest {
                request_id: "4f2a1c3e-5b6d-4e7f-8a9b-0c1d2e3f4a5b".to_owned(),
                workspace,
                correlation: None,
                operation,
            }))
            .await
        else {
            panic!("expected a Query response")
        };
        response.outcome
    }

    async fn query(
        &self,
        workspace: WorkspaceId,
        operation: QueryOperationWire,
    ) -> QueryResultWire {
        match self
            .query_outcome(
                WorkspaceSelectorWire::Id {
                    workspace_id: workspace,
                },
                operation,
            )
            .await
        {
            QueryOutcomeWire::Ok(result) => result,
            QueryOutcomeWire::Err(error) => panic!("query error: {error:?}"),
        }
    }

    /// `inspect` a Symbol: its delivered current source, if resolved and
    /// current.
    async fn inspect(&self, workspace: WorkspaceId, name: &str) -> Option<String> {
        let QueryResultWire::Inspect(answer) = self.query(workspace, inspect_op(name)).await else {
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

    /// ACTIVE file paths, sorted, and whether the listing is current.
    async fn files(&self, workspace: WorkspaceId) -> (Vec<String>, bool) {
        let QueryResultWire::Find(FindResultWire::Files(listing)) =
            self.query(workspace, files_op()).await
        else {
            panic!("expected a file listing")
        };
        let current = listing.currentness == CurrentnessWire::Current;
        let mut paths: Vec<String> = listing
            .entries
            .into_iter()
            .map(|entry| entry.path_rel)
            .collect();
        paths.sort();
        (paths, current)
    }

    /// Direct incoming callers of `name`.
    async fn callers(&self, workspace: WorkspaceId, name: &str) -> usize {
        let result = self
            .query(
                workspace,
                QueryOperationWire::Relations(RelationsWire {
                    target: symbol(name),
                    direction: RelationDirectionWire::Incoming,
                    kinds: vec![RelationKindWire::Calls],
                }),
            )
            .await;
        let QueryResultWire::Relations(answer) = result else {
            panic!("expected direct relations")
        };
        answer
            .answers
            .iter()
            .map(|answer| answer.confirmed.len())
            .sum()
    }
}

fn symbol(name: &str) -> ProjectionTargetWire {
    ProjectionTargetWire::Symbol(SymbolTargetWire {
        name: SymbolNameWire::Name(name.to_owned()),
        resource: None,
        kind: None,
        language: None,
    })
}

fn inspect_op(name: &str) -> QueryOperationWire {
    QueryOperationWire::Inspect(InspectWire {
        target: symbol(name),
        delivery: DeliveryWire {
            budget: DeliveryBudgetWire {
                max_items: NonZeroUsize::new(64),
                max_bytes: NonZeroUsize::new(64 * 1024),
            },
            continuation: None,
            retention: RetentionWire::Disabled,
        },
    })
}

fn files_op() -> QueryOperationWire {
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

/// The sync's refresh fact: (before, after) generation and the counts.
struct Synced {
    before_generation: i64,
    after_generation: i64,
    before_revision: String,
    after_revision: String,
    created: u64,
    updated: u64,
    deleted: u64,
    changes: Vec<(ResourceDeltaKindWire, String)>,
}

fn synced(response: &SyncResponse) -> Synced {
    let PostCommandRefreshWire::Current {
        before,
        after,
        created_count,
        updated_count,
        deleted_count,
        total_changed,
        changes,
        delivery_omitted,
    } = &response.refresh
    else {
        panic!("a sync is always Current: {:?}", response.refresh)
    };
    assert_eq!(
        *total_changed,
        created_count + updated_count + deleted_count
    );
    assert_eq!(*delivery_omitted, 0);
    assert_eq!(before.index_incarnation, after.index_incarnation);
    Synced {
        before_generation: before.generation_no,
        after_generation: after.generation_no,
        before_revision: before.workspace_revision.clone(),
        after_revision: after.workspace_revision.clone(),
        created: *created_count,
        updated: *updated_count,
        deleted: *deleted_count,
        changes: changes
            .iter()
            .map(|change| (change.kind, change.path.clone()))
            .collect(),
    }
}

/// Every file under `dir` (recursively) with its bytes, skipping
/// `.brainprint/` (Brainprint's own state, checked separately).
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in fs::read_dir(&next).expect("read_dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            if entry.file_name() == ".brainprint" {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else {
                files.insert(path.clone(), fs::read(&path).expect("read"));
            }
        }
    }
    files
}

/// Every row of every table of `db`, sorted. Rows, not file bytes:
/// closing the last connection checkpoints the WAL, which moves bytes
/// without changing content.
fn rows(db: &Path) -> BTreeMap<String, Vec<String>> {
    let connection = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("db");
    let tables: Vec<String> = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .expect("tables")
        .query_map([], |row| row.get(0))
        .expect("tables")
        .collect::<Result<_, _>>()
        .expect("tables");
    let mut state = BTreeMap::new();
    for table in tables {
        let mut statement = connection
            .prepare(&format!("SELECT * FROM \"{table}\""))
            .expect("select");
        let columns = statement.column_count();
        let mut found: Vec<String> = statement
            .query_map([], |row| {
                (0..columns)
                    .map(|index| row.get::<_, rusqlite::types::Value>(index))
                    .collect::<Result<Vec<_>, _>>()
                    .map(|values| format!("{values:?}"))
            })
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("rows");
        found.sort();
        state.insert(table, found);
    }
    state
}

/// The durable side of a Workspace: identity and config text, and every
/// row of its workspace.db and (when it is the home) project.db.
fn durable(root: &Path) -> BTreeMap<String, Vec<String>> {
    let paths = WorkspacePaths::from_root(root);
    let mut state = BTreeMap::new();
    for file in [&paths.identity_file, &paths.config_file] {
        let text = fs::read_to_string(file).expect("read");
        state.insert(file.display().to_string(), vec![text]);
    }
    for db in [&paths.project_db, &paths.workspace_db] {
        if !db.is_file() {
            continue;
        }
        for (table, found) in rows(db) {
            state.insert(format!("{}:{table}", db.display()), found);
        }
    }
    state
}

fn generations(root: &Path) -> Vec<(i64, String)> {
    let connection = Connection::open_with_flags(
        WorkspacePaths::from_root(root).index_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("index.db");
    let mut statement = connection
        .prepare("SELECT generation_no, state FROM generation ORDER BY id")
        .expect("prepare");
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows")
}

fn registry_state(global: &GlobalPaths, workspace: WorkspaceId) -> String {
    Connection::open_with_flags(&global.global_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("global.db")
        .query_row(
            "SELECT state FROM workspace_registry WHERE workspace_uid = ?1",
            [workspace.to_bytes().to_vec()],
            |row| row.get(0),
        )
        .expect("registry row")
}

fn seed_policy(project_home: &Path, key: &str) {
    ProjectKnowledgeStore::open(&WorkspacePaths::from_root(project_home).project_db)
        .expect("project.db")
        .insert_policy(&NewPolicy {
            scope: brainprint_engine::knowledge::KnowledgeScope::project(),
            policy_key: Some(key.to_owned()),
            title: "keep helper() pure".to_owned(),
            rule_text: "helper() must stay side-effect-free".to_owned(),
            structured_rule: None,
            protection_class: ProtectionClass::Normal,
            priority_class: PriorityClass::Default,
            provenance: Provenance::new(SourceKind::UserExplicit),
        })
        .expect("policy");
}

fn seed_work_item(root: &Path, workspace: WorkspaceId, goal: &str) {
    let paths = WorkspacePaths::from_root(root);
    WorkRuntime::open(workspace, &paths.workspace_db, &paths.index_db)
        .expect("work runtime")
        .create(&NewWorkItem {
            source_kind: WorkItemSourceKind::Issue,
            source_ref: Some("#57".to_owned()),
            title: None,
            goal: goal.to_owned(),
        })
        .expect("work item");
}

fn not_initialized(outcome: &QueryOutcomeWire) -> Option<&NotInitializedReasonWire> {
    match outcome {
        QueryOutcomeWire::Err(QueryErrorWire {
            detail: Some(QueryErrorDetailWire::NotInitialized(reason)),
            ..
        }) => Some(reason),
        _ => None,
    }
}

fn git(args: &[&str], cwd: &Path) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "brainprint-test")
        .env("GIT_AUTHOR_EMAIL", "test@brainprint.invalid")
        .env("GIT_COMMITTER_NAME", "brainprint-test")
        .env("GIT_COMMITTER_EMAIL", "test@brainprint.invalid")
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ================================================================= sync

#[tokio::test]
async fn a_no_op_sync_is_current_with_no_changes_and_starts_nothing() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global, None).await;
    let workspace = fixture_workspace("noop");
    let id = daemon.init_id(workspace.path()).await;
    let generations_before = generations(workspace.path());

    for _ in 0..2 {
        let response = daemon.sync(workspace.path()).await.expect("sync");
        assert_eq!(response.workspace_id, id.to_string());
        assert_eq!(response.watcher, WatcherCheckWire::Attached);
        let synced = synced(&response);
        assert_eq!((synced.created, synced.updated, synced.deleted), (0, 0, 0));
        assert_eq!(synced.before_generation, synced.after_generation);
        assert_eq!(synced.before_revision, synced.after_revision);
    }
    // No publication, no revision move, no backend started.
    assert_eq!(generations(workspace.path()), generations_before);
    let semantic = daemon.runtime.semantic_stats(id).await.expect("semantic");
    assert_eq!(semantic.backend_starts, 0);
    assert!(semantic.registered.is_empty(), "no backend in this home");
    daemon.stop().await;
}

#[tokio::test]
async fn sync_recovers_what_the_watcher_missed_and_only_in_its_workspace() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global, Some(deaf())).await;
    let workspace = fixture_workspace("missed");
    let other = fixture_workspace("missed-other");
    let id = daemon.init_id(workspace.path()).await;
    let other_id = daemon.init_id(other.path()).await;
    assert_eq!(daemon.callers(id, "helper").await, 1);

    // Create, update and delete, plus a bulk create -- none of which the
    // watcher reports. Same edits in the other Workspace.
    for root in [workspace.path(), other.path()] {
        let src = root.join("src");
        fs::write(
            src.join("shared.ts"),
            "export function helperTwo(): number {\n  return 7;\n}\n",
        )
        .expect("update");
        fs::remove_file(src.join("app.ts")).expect("delete");
        fs::write(
            src.join("extra.ts"),
            "export function extra(): number {\n  return 1;\n}\n",
        )
        .expect("create");
        let bulk = src.join("bulk");
        fs::create_dir_all(&bulk).expect("bulk");
        for index in 0..40 {
            fs::write(
                bulk.join(format!("b{index}.ts")),
                format!("export const b{index} = {index};\n"),
            )
            .expect("bulk file");
        }
    }

    // The watcher kept claiming continuity: the index still shows the old
    // tree.
    let (stale, _) = daemon.files(id).await;
    assert!(stale.contains(&"src/app.ts".to_owned()), "{stale:?}");
    assert!(daemon.inspect(id, "extra").await.is_none());

    let response = daemon.sync(workspace.path()).await.expect("sync");
    let synced = synced(&response);
    // Created: extra.ts, the src/bulk directory and its 40 files.
    assert_eq!((synced.created, synced.updated, synced.deleted), (42, 1, 1));
    assert!(
        synced
            .changes
            .contains(&(ResourceDeltaKindWire::Created, "src/bulk".to_owned()))
    );
    assert!(synced.after_generation > synced.before_generation);
    assert!(
        synced
            .changes
            .contains(&(ResourceDeltaKindWire::Deleted, "src/app.ts".to_owned()))
    );
    assert!(
        synced
            .changes
            .contains(&(ResourceDeltaKindWire::Updated, "src/shared.ts".to_owned()))
    );
    assert!(
        synced
            .changes
            .contains(&(ResourceDeltaKindWire::Created, "src/extra.ts".to_owned()))
    );

    // At once, no sleep, restart or rebuild: find / inspect / relations
    // see the current tree.
    let (files, current) = daemon.files(id).await;
    assert!(current);
    assert_eq!(files.len(), 42, "{files:?}");
    assert!(!files.contains(&"src/app.ts".to_owned()));
    assert!(files.contains(&"src/bulk/b39.ts".to_owned()));
    assert!(
        daemon
            .inspect(id, "extra")
            .await
            .expect("extra")
            .contains("return 1")
    );
    assert!(
        daemon
            .inspect(id, "helperTwo")
            .await
            .expect("helperTwo")
            .contains("return 7")
    );
    assert_eq!(daemon.callers(id, "helperTwo").await, 0);

    // The other Workspace was not synced: still the old tree.
    let (other_files, _) = daemon.files(other_id).await;
    assert!(other_files.contains(&"src/app.ts".to_owned()));
    assert_eq!(other_files.len(), 2);

    // Repeating is a no-op.
    let again = self::synced(&daemon.sync(workspace.path()).await.expect("sync"));
    assert_eq!((again.created, again.updated, again.deleted), (0, 0, 0));
    assert_eq!(again.before_generation, again.after_generation);
    daemon.stop().await;
}

#[tokio::test]
async fn sync_after_daemon_downtime_reports_what_it_recovered() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global, None).await;
    let workspace = fixture_workspace("downtime");
    let id = daemon.init_id(workspace.path()).await;
    daemon.stop().await;

    // Changed while no daemon ran.
    fs::remove_file(workspace.path().join("src/app.ts")).expect("delete");
    fs::write(
        workspace.path().join("src/late.ts"),
        "export function late(): number {\n  return 3;\n}\n",
    )
    .expect("create");

    // The fresh daemon holds no runtime; sync activates it and the delta
    // is measured from the stored generation, not from after activation.
    let daemon = Daemon::start(&global, None).await;
    assert_eq!(daemon.runtime.workspace_runtime_count(), 0);
    let synced = synced(&daemon.sync(workspace.path()).await.expect("sync"));
    assert_eq!((synced.created, synced.updated, synced.deleted), (1, 0, 1));
    let (files, current) = daemon.files(id).await;
    assert!(current);
    assert_eq!(files, ["src/late.ts", "src/shared.ts"]);
    daemon.stop().await;
}

#[tokio::test]
async fn sync_refuses_an_uninitialized_folder_and_creates_nothing() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global, None).await;
    let plain = fixture_workspace("plain");
    let error = daemon.sync(plain.path()).await.expect_err("refused");
    assert_eq!(error.kind, ErrorKind::InvalidRequest);
    assert!(error.message.contains("not initialized"), "{error:?}");
    assert!(!plain.path().join(".brainprint").exists());
    assert_eq!(daemon.runtime.workspace_runtime_count(), 0);
    daemon.stop().await;
}

// =============================================================== uninit

#[tokio::test]
async fn uninit_detaches_keeps_everything_and_init_attaches_again() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global, None).await;
    let workspace = fixture_workspace("uninit");
    let first = daemon.init(workspace.path()).await;
    let id: WorkspaceId = first.workspace_id.parse().expect("id");
    seed_policy(workspace.path(), "i7-task2-policy");
    seed_work_item(workspace.path(), id, "survive an uninit");
    assert!(daemon.runtime.lifecycle_stats(id).await.is_some());

    let source = snapshot(workspace.path());
    let durable_before = durable(workspace.path());
    let index_before = rows(&WorkspacePaths::from_root(workspace.path()).index_db);

    let response = daemon.uninit(workspace.path()).await.expect("uninit");
    assert_eq!(response.workspace_id, first.workspace_id);
    assert_eq!(response.project_id, first.project_id);
    assert!(response.is_project_home);
    assert!(!response.already_detached);
    assert!(response.runtime_stopped);

    // Runtime and watcher are gone; the registry says DETACHED.
    assert_eq!(daemon.runtime.workspace_runtime_count(), 0);
    assert!(daemon.runtime.lifecycle_stats(id).await.is_none());
    assert_eq!(registry_state(&global, id), "DETACHED");

    // Nothing was touched: source bytes, identity/config, durable rows,
    // index rows.
    assert_eq!(snapshot(workspace.path()), source);
    assert_eq!(durable(workspace.path()), durable_before);
    assert_eq!(
        rows(&WorkspacePaths::from_root(workspace.path()).index_db),
        index_before
    );

    // It no longer answers as initialized -- by path or by id.
    let by_path = daemon
        .query_outcome(
            WorkspaceSelectorWire::Locator {
                path: path(workspace.path()),
            },
            files_op(),
        )
        .await;
    assert_eq!(
        not_initialized(&by_path),
        Some(&NotInitializedReasonWire::WorkspaceDetached),
        "{by_path:?}"
    );
    let by_id = daemon
        .query_outcome(WorkspaceSelectorWire::Id { workspace_id: id }, files_op())
        .await;
    assert_eq!(
        not_initialized(&by_id),
        Some(&NotInitializedReasonWire::WorkspaceDetached),
        "{by_id:?}"
    );
    // ... and that lookup bound no runtime: no watcher came back.
    assert!(daemon.runtime.lifecycle_stats(id).await.is_none());
    let Response::Doctor(doctor) = daemon
        .send(Request::Doctor(DoctorRequest {
            path: path(workspace.path()),
        }))
        .await
    else {
        panic!("expected a Doctor response")
    };
    assert!(matches!(
        doctor.workspace,
        DoctorWorkspaceWire::NotInitialized { .. }
    ));
    assert!(daemon.sync(workspace.path()).await.is_err());
    let Response::Error(_) = daemon
        .send(Request::Rebuild(RebuildRequest {
            path: path(workspace.path()),
        }))
        .await
    else {
        panic!("a detached Workspace is not rebuilt")
    };

    // Uninit again: already detached, nothing changes.
    let again = daemon.uninit(workspace.path()).await.expect("uninit");
    assert!(again.already_detached);
    assert_eq!(again.workspace_id, first.workspace_id);
    assert_eq!(registry_state(&global, id), "DETACHED");

    // A change made while detached is picked up by the explicit init.
    fs::write(
        workspace.path().join("src/after.ts"),
        "export function after(): number {\n  return 9;\n}\n",
    )
    .expect("create");
    let source = snapshot(workspace.path());

    let reinit = daemon.init(workspace.path()).await;
    assert_eq!(reinit.workspace_id, first.workspace_id);
    assert_eq!(reinit.project_id, first.project_id);
    assert!(!reinit.freshly_created);
    assert_eq!(registry_state(&global, id), "ACTIVE");
    assert_eq!(durable(workspace.path()), durable_before);
    assert_eq!(snapshot(workspace.path()), source);
    let (files, current) = daemon.files(id).await;
    assert!(current);
    assert_eq!(files, ["src/after.ts", "src/app.ts", "src/shared.ts"]);
    assert_eq!(daemon.callers(id, "helper").await, 1);
    let stats = daemon.runtime.lifecycle_stats(id).await.expect("runtime");
    assert!(stats.watcher_attached);
    daemon.stop().await;
}

#[tokio::test]
async fn uninit_refuses_an_uninitialized_folder() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global, None).await;
    let plain = fixture_workspace("uninit-plain");
    let source = snapshot(plain.path());
    let error = daemon.uninit(plain.path()).await.expect_err("refused");
    assert_eq!(error.kind, ErrorKind::InvalidRequest);
    assert!(error.message.contains("not initialized"), "{error:?}");
    assert_eq!(snapshot(plain.path()), source);
    assert!(!plain.path().join(".brainprint").exists());
    daemon.stop().await;
}

#[tokio::test]
async fn uninit_of_the_project_home_leaves_its_worktree_untouched() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global, None).await;

    let main = TestDir::create("wt-main");
    write_sources(main.path());
    git(&["init", "-q"], main.path());
    git(&["add", "src"], main.path());
    git(&["commit", "-q", "-m", "init"], main.path());
    let parent = TestDir::create("wt-parent");
    let secondary = parent.path().join("feature");
    git(
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            &secondary.to_string_lossy(),
        ],
        main.path(),
    );
    let secondary = secondary.canonicalize().expect("canonical");

    let a = daemon.init(main.path()).await;
    let b = daemon.init(&secondary).await;
    assert_eq!(a.project_id, b.project_id);
    let a_id: WorkspaceId = a.workspace_id.parse().expect("id");
    let b_id: WorkspaceId = b.workspace_id.parse().expect("id");
    seed_policy(main.path(), "i7-task2-shared");
    seed_work_item(&secondary, b_id, "keep working in B");
    assert_eq!(daemon.callers(b_id, "helper").await, 1);

    let project_rows = rows(&WorkspacePaths::from_root(main.path()).project_db);
    let b_durable = durable(&secondary);
    let b_source = snapshot(&secondary);
    let b_stats = daemon.runtime.lifecycle_stats(b_id).await.expect("B");

    let response = daemon.uninit(main.path()).await.expect("uninit A");
    assert!(response.is_project_home);
    assert_eq!(registry_state(&global, a_id), "DETACHED");

    // B: identity, registry state, runtime, Working State, source --
    // and the Project's durable knowledge it reads from A's home.
    assert_eq!(registry_state(&global, b_id), "ACTIVE");
    assert_eq!(durable(&secondary), b_durable);
    assert_eq!(snapshot(&secondary), b_source);
    assert_eq!(
        rows(&WorkspacePaths::from_root(main.path()).project_db),
        project_rows
    );
    let still = daemon.runtime.lifecycle_stats(b_id).await.expect("B runs");
    assert!(still.watcher_attached);
    assert_eq!(
        still.watcher_attach_attempts,
        b_stats.watcher_attach_attempts
    );
    assert_eq!(daemon.callers(b_id, "helper").await, 1);
    let (files, current) = daemon.files(b_id).await;
    assert!(current);
    // A secondary worktree's `.git` is a gitlink file.
    assert_eq!(files, [".git", "src/app.ts", "src/shared.ts"]);
    // B's own sync still works with A detached.
    let synced = synced(&daemon.sync(&secondary).await.expect("sync B"));
    assert_eq!((synced.created, synced.updated, synced.deleted), (0, 0, 0));
    let QueryResultWire::Knowledge(_) = daemon
        .query(
            b_id,
            QueryOperationWire::Knowledge(KnowledgeWire::WorkItems {
                statuses: vec![WorkItemStatusWire::Open],
                limit: NonZeroUsize::new(10).expect("nz"),
            }),
        )
        .await
    else {
        panic!("expected B's knowledge")
    };

    // A comes back with the same identity and B is unaffected again.
    let again = daemon.init(main.path()).await;
    assert_eq!(again.workspace_id, a.workspace_id);
    assert_eq!(registry_state(&global, a_id), "ACTIVE");
    assert_eq!(registry_state(&global, b_id), "ACTIVE");
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
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .expect("brainprint")
}

#[tokio::test]
async fn the_cli_runs_sync_and_uninit() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global, Some(deaf())).await;
    let workspace = fixture_workspace("cli");
    let id = daemon.init_id(workspace.path()).await;
    fs::write(
        workspace.path().join("src/cli.ts"),
        "export function viaCli(): number {\n  return 5;\n}\n",
    )
    .expect("create");

    let run = |args: Vec<String>| {
        let home = home.path().to_path_buf();
        tokio::task::spawn_blocking(move || {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            cli(&home, &args)
        })
    };
    let ws = path(workspace.path());
    let has = |text: &str, expected: &[&str]| {
        text.lines()
            .any(|line| line.split_whitespace().eq(expected.iter().copied()))
    };

    let sync = run(vec!["sync".into(), ws.clone()]).await.expect("cli");
    let text = String::from_utf8_lossy(&sync.stdout).into_owned();
    assert_eq!(sync.status.code(), Some(0), "{text}");
    assert!(text.contains(&id.to_string()), "{text}");
    assert!(
        has(
            &text,
            &[
                "changes:", "1", "(created", "1,", "updated", "0,", "deleted", "0)"
            ]
        ),
        "{text}"
    );
    assert!(has(&text, &["created", "src/cli.ts"]), "{text}");
    assert!(has(&text, &["index:", "current"]), "{text}");
    assert!(text.lines().count() < 30, "compact:\n{text}");
    assert!(daemon.inspect(id, "viaCli").await.is_some());

    let json = run(vec!["sync".into(), ws.clone(), "--json".into()])
        .await
        .expect("cli");
    let parsed: SyncResponse = serde_json::from_slice(&json.stdout).expect("sync json");
    let parsed = synced(&parsed);
    assert_eq!((parsed.created, parsed.updated, parsed.deleted), (0, 0, 0));

    let uninit = run(vec!["uninit".into(), ws.clone()]).await.expect("cli");
    let text = String::from_utf8_lossy(&uninit.stdout).into_owned();
    assert_eq!(uninit.status.code(), Some(0), "{text}");
    assert!(text.starts_with("detached:"), "{text}");
    assert_eq!(registry_state(&global, id), "DETACHED");

    let doctor = run(vec!["doctor".into(), ws.clone()]).await.expect("cli");
    assert_eq!(doctor.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("not initialized"));

    let again = run(vec!["uninit".into(), ws.clone()]).await.expect("cli");
    let text = String::from_utf8_lossy(&again.stdout).into_owned();
    assert_eq!(again.status.code(), Some(0), "{text}");
    assert!(text.starts_with("already detached:"), "{text}");

    let init = run(vec!["init".into(), ws.clone()]).await.expect("cli");
    assert_eq!(
        init.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    assert_eq!(registry_state(&global, id), "ACTIVE");
    assert!(daemon.inspect(id, "viaCli").await.is_some());
    daemon.stop().await;
}
