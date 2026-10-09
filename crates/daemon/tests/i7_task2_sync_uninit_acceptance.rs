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
        InitResponse, InstallRequest, Request, Response, StatusRequest, StatusResponse,
        maintenance::{
            DoctorRequest, DoctorWorkspaceWire, IndexCheckWire, RebuildRequest, RuntimeCheckWire,
            StoredBasisWire, SyncRequest, SyncResponse, UninitRequest, UninitResponse,
            WatcherCheckWire, WorkspaceStatusReportWire, WorkspaceStatusWire,
        },
        query::*,
        work::{IndexBasisWire, PostCommandRefreshWire, ResourceDeltaKindWire},
    },
};
use brainprint_daemon::{
    query::{DaemonQueryRuntime, lifecycle::WatchFactory},
    runtime_paths,
    server::Server,
};
use brainprint_engine::{
    db::{self, DbKind, Migration},
    generation::{GenerationState, GenerationStore},
    knowledge::{
        NewPolicy, NewWorkItem, PriorityClass, ProjectKnowledgeStore, ProtectionClass, Provenance,
        SourceKind, WorkItemSourceKind, WorkRuntime,
    },
    paths::{GlobalPaths, WorkspacePaths},
    schema,
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
                    delivery: inspect_op_delivery(),
                }),
            )
            .await;
        let QueryResultWire::Relations(answer) = result else {
            panic!("expected direct relations")
        };
        answer.totals.iter().map(|totals| totals.confirmed).sum()
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
        delivery: inspect_op_delivery(),
    })
}

fn inspect_op_delivery() -> DeliveryWire {
    DeliveryWire {
        budget: DeliveryBudgetWire {
            max_items: NonZeroUsize::new(64),
            max_bytes: NonZeroUsize::new(64 * 1024),
        },
        continuation: None,
        retention: RetentionWire::Disabled,
    }
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

/// A committed project home and a secondary worktree of it: (main, the
/// secondary's parent dir to keep alive, the secondary's root).
fn worktree_pair() -> (TestDir, TestDir, PathBuf) {
    let main = TestDir::create("wt-main");
    write_sources(main.path());
    git(&["init", "-q"], main.path());
    git(&["add", "src"], main.path());
    git(&["commit", "-q", "-m", "init"], main.path());
    let parent = TestDir::create("wt-parent");
    let secondary = parent.path().join("feature");
    // Git cannot create directories under a Windows verbatim (`\\?\`)
    // path, which `canonicalize` returns there.
    let target = secondary.to_string_lossy().into_owned();
    let target = target.strip_prefix(r"\\?\").unwrap_or(&target);
    git(
        &["worktree", "add", "-q", "-b", "feature", target],
        main.path(),
    );
    let secondary = secondary.canonicalize().expect("canonical");
    (main, parent, secondary)
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
    let (main, _parent, secondary) = worktree_pair();

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

// ================================================= #64 recovery / worktree

/// The Working State a Workspace's own query reports, as text.
async fn work_items(daemon: &Daemon, workspace: WorkspaceId) -> String {
    let result = daemon
        .query(
            workspace,
            QueryOperationWire::Knowledge(KnowledgeWire::WorkItems {
                statuses: vec![WorkItemStatusWire::Open],
                limit: NonZeroUsize::new(10).expect("nz"),
            }),
        )
        .await;
    format!("{result:?}")
}

/// The Project rules a Workspace's own query reports, as text.
async fn rules(daemon: &Daemon, workspace: WorkspaceId) -> String {
    let result = daemon
        .query(
            workspace,
            QueryOperationWire::Knowledge(KnowledgeWire::Rules {
                scope_layers: Vec::new(),
                directives: Vec::new(),
                knowledge_refs: ProjectionKnowledgeRefsWire::default(),
            }),
        )
        .await;
    format!("{result:?}")
}

/// #64: a crash left a BUILDING generation behind and the tree changed
/// while no daemon ran. The first plain query after restart -- no init,
/// sync, rebuild or sleep -- serves the filesystem truth from a new STABLE
/// generation; the orphan is never current; Working State survives; the
/// watcher is attached again.
#[tokio::test]
async fn a_restart_after_a_crash_mid_generation_serves_the_filesystem_truth() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let workspace = fixture_workspace("crash");
    let root = workspace.path();
    let daemon = Daemon::start(&global, None).await;
    let id = daemon.init_id(root).await;
    seed_work_item(root, id, "survive the crash");
    daemon.stop().await;

    // The crash: a generation begun and never published or aborted.
    let index = WorkspacePaths::from_root(root).index_db;
    let (stable, orphan) = {
        let store = GenerationStore::open(&index).expect("index.db");
        let stable = store.current_stable().expect("current").expect("stable");
        let orphan = store
            .begin_generation(&stable.basis_workspace_revision)
            .expect("begin");
        (stable, orphan)
    };
    let work = WorkspacePaths::from_root(root).workspace_db;
    let knowledge = rows(&work);
    fs::write(
        root.join("src/shared.ts"),
        "export function helper(): number {\n  return 7;\n}\n",
    )
    .expect("modify");
    fs::remove_file(root.join("src/app.ts")).expect("delete");
    fs::write(
        root.join("src/late.ts"),
        "export function late(): number {\n  return 3;\n}\n",
    )
    .expect("create");

    let daemon = Daemon::start(&global, None).await;
    let (files, current) = daemon.files(id).await;
    assert!(current);
    assert_eq!(files, ["src/late.ts", "src/shared.ts"]);
    assert!(
        daemon
            .inspect(id, "helper")
            .await
            .expect("helper")
            .contains("return 7")
    );
    assert!(daemon.inspect(id, "late").await.is_some());
    assert!(daemon.inspect(id, "run").await.is_none());

    let store = GenerationStore::open(&index).expect("index.db");
    let now = store.current_stable().expect("current").expect("stable");
    assert!(now.generation_no > stable.generation_no);
    assert_ne!(now.id, orphan.id, "the orphan is never current");
    let orphan = store.get_generation(orphan.id).expect("get").expect("row");
    // Lazy activation leaves the orphan BUILDING (only `init` aborts it);
    // what matters is that it never becomes current.
    assert_ne!(orphan.state, GenerationState::Stable);
    assert_eq!(rows(&work), knowledge, "Working State survives");
    assert!(work_items(&daemon, id).await.contains("survive the crash"));

    // The watcher is back: a live edit is seen without a sync.
    let stats = daemon.runtime.lifecycle_stats(id).await.expect("runtime");
    assert!(stats.watcher_attached);
    fs::write(
        root.join("src/live.ts"),
        "export function live(): number {\n  return 5;\n}\n",
    )
    .expect("live edit");
    let mut seen = false;
    for _ in 0..100 {
        if daemon.inspect(id, "live").await.is_some() {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(seen, "the restarted watcher reports a live edit");
    daemon.stop().await;
}

/// #64: worktree A's edit, sync, rebuild, uninit/re-init and a daemon
/// restart never move worktree B's generations, Working State, source or
/// watcher -- and B's sync never moves A. Project knowledge stays shared.
#[tokio::test]
async fn one_worktrees_lifecycle_never_moves_the_other() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global, None).await;
    let (main, _parent, secondary) = worktree_pair();
    let a = daemon.init(main.path()).await;
    let b = daemon.init(&secondary).await;
    assert_eq!(a.project_id, b.project_id);
    assert_ne!(a.workspace_id, b.workspace_id);
    let a_id: WorkspaceId = a.workspace_id.parse().expect("id");
    let b_id: WorkspaceId = b.workspace_id.parse().expect("id");
    seed_policy(main.path(), "i7-task3-shared");
    seed_work_item(main.path(), a_id, "work only in A");
    seed_work_item(&secondary, b_id, "work only in B");

    let b_state = || {
        (
            generations(&secondary),
            durable(&secondary),
            snapshot(&secondary),
        )
    };
    let b_before = b_state();
    let b_watch = daemon.runtime.lifecycle_stats(b_id).await.expect("B");

    // A: edit + sync, rebuild, uninit + init.
    fs::write(
        main.path().join("src/only_a.ts"),
        "export function onlyA(): number {\n  return 1;\n}\n",
    )
    .expect("edit A");
    let a_synced = synced(&daemon.sync(main.path()).await.expect("sync A"));
    assert!(a_synced.after_generation > a_synced.before_generation);
    let Response::Rebuild(_) = daemon
        .send(Request::Rebuild(RebuildRequest {
            path: path(main.path()),
        }))
        .await
    else {
        panic!("rebuild A")
    };
    daemon.uninit(main.path()).await.expect("uninit A");
    daemon.init(main.path()).await;

    assert_eq!(b_state(), b_before);
    let b_still = daemon.runtime.lifecycle_stats(b_id).await.expect("B runs");
    assert!(b_still.watcher_attached);
    assert_eq!(
        b_still.watcher_attach_attempts,
        b_watch.watcher_attach_attempts
    );
    assert!(daemon.inspect(b_id, "onlyA").await.is_none());

    // A daemon restart: B comes back on its own query, unchanged.
    daemon.stop().await;
    let daemon = Daemon::start(&global, None).await;
    assert!(daemon.inspect(a_id, "onlyA").await.is_some());
    let (b_files, current) = daemon.files(b_id).await;
    assert!(current);
    assert_eq!(b_files, [".git", "src/app.ts", "src/shared.ts"]);
    assert_eq!(b_state(), b_before);

    // Working State is per Workspace; the Project's rules are shared.
    let (a_work, b_work) = (
        work_items(&daemon, a_id).await,
        work_items(&daemon, b_id).await,
    );
    assert!(a_work.contains("work only in A") && !a_work.contains("work only in B"));
    assert!(b_work.contains("work only in B") && !b_work.contains("work only in A"));
    for workspace in [a_id, b_id] {
        assert!(
            rules(&daemon, workspace)
                .await
                .contains("keep helper() pure")
        );
    }

    // The other direction: B's edit + sync never moves A.
    let a_generations = generations(main.path());
    fs::write(
        secondary.join("src/only_b.ts"),
        "export function onlyB(): number {\n  return 2;\n}\n",
    )
    .expect("edit B");
    let b_synced = synced(&daemon.sync(&secondary).await.expect("sync B"));
    assert!(b_synced.after_generation > b_synced.before_generation);
    assert!(daemon.inspect(b_id, "onlyB").await.is_some());
    assert!(daemon.inspect(a_id, "onlyB").await.is_none());
    assert_eq!(generations(main.path()), a_generations);
    daemon.stop().await;
}

/// #64: a file in no supported language is still a Resource -- listed,
/// text-searchable, synced -- and its relations say Unsupported, never a
/// complete zero. No backend is configured, so this is also the "backend
/// missing" Workspace: the TypeScript next to it stays structurally usable.
#[tokio::test]
async fn an_unsupported_language_file_is_usable_and_never_a_false_zero() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global, Some(deaf())).await;
    let workspace = fixture_workspace("unsupported");
    let root = workspace.path();
    fs::write(
        root.join("src/main.go"),
        "package main\n\nfunc gopherOnly() int { return helper() }\n",
    )
    .expect("go file");
    let id = daemon.init_id(root).await;

    let (files, current) = daemon.files(id).await;
    assert!(current);
    assert_eq!(files, ["src/app.ts", "src/main.go", "src/shared.ts"]);
    let text = daemon
        .query(
            id,
            QueryOperationWire::Find(FindQueryWire::Text {
                pattern: TextPatternWire::Literal("gopherOnly".to_owned()),
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
        .await;
    let text = format!("{text:?}");
    assert!(
        text.contains("src/main.go") && text.contains("func gopherOnly()"),
        "{text}"
    );

    let QueryResultWire::Relations(answer) = daemon
        .query(
            id,
            QueryOperationWire::Relations(RelationsWire {
                target: ProjectionTargetWire::Resource(ResourceTargetWire::Path(
                    "src/main.go".to_owned(),
                )),
                direction: RelationDirectionWire::Outgoing,
                kinds: Vec::new(),
                delivery: inspect_op_delivery(),
            }),
        )
        .await
    else {
        panic!("expected direct relations")
    };
    for totals in &answer.totals {
        assert_eq!(totals.confirmed, 0);
        let scope = totals.coverage.scope.as_ref().expect("scoped coverage");
        assert_eq!(scope.support, SupportWire::Unsupported, "{totals:?}");
    }
    assert!(daemon.inspect(id, "gopherOnly").await.is_none());

    // The supported files beside it keep their structural truth.
    assert_eq!(daemon.callers(id, "helper").await, 1);

    // An edit to it is synced like any other Resource.
    fs::write(root.join("src/main.go"), "package main\n").expect("edit");
    let synced = synced(&daemon.sync(root).await.expect("sync"));
    assert_eq!((synced.created, synced.updated, synced.deleted), (0, 1, 0));
    daemon.stop().await;
}

/// #68: five clients split 3 + 2 over two worktrees of one Project, no
/// semantic backend configured. All five query at once; A's edit + sync
/// and then B's never move the other side; structural answers stay
/// current; the missing backend is never started or retried per client.
#[tokio::test]
async fn three_and_two_clients_on_two_worktrees_stay_isolated_without_a_backend() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global, Some(deaf())).await;
    let (main, _parent, secondary) = worktree_pair();
    let a = daemon.init(main.path()).await;
    let b = daemon.init(&secondary).await;
    assert_eq!(a.project_id, b.project_id);
    let a_id: WorkspaceId = a.workspace_id.parse().expect("id");
    let b_id: WorkspaceId = b.workspace_id.parse().expect("id");
    seed_policy(main.path(), "i7-task5-shared");
    seed_work_item(main.path(), a_id, "work only in A");
    seed_work_item(&secondary, b_id, "work only in B");

    // Clients 1-3 on A, 4-5 on B, all at once.
    let all_five = || async {
        tokio::join!(
            daemon.files(a_id),
            daemon.inspect(a_id, "run"),
            daemon.callers(a_id, "helper"),
            daemon.files(b_id),
            work_items(&daemon, b_id),
        )
    };
    let (a_files, a_run, a_callers, b_files, b_work) = all_five().await;
    assert!(a_files.1 && b_files.1);
    assert!(a_run.expect("run").contains("return helper()"));
    assert_eq!(a_callers, 1, "structural relations without a backend");
    assert!(b_work.contains("work only in B") && !b_work.contains("work only in A"));
    for workspace in [a_id, b_id] {
        assert!(
            rules(&daemon, workspace)
                .await
                .contains("keep helper() pure")
        );
    }

    // A changes and syncs while B's two clients keep querying.
    let b_before = generations(&secondary);
    fs::write(
        main.path().join("src/only_a.ts"),
        "export function onlyA(): number {\n  return 1;\n}\n",
    )
    .expect("edit A");
    let (a_sync, b_files, b_work) = tokio::join!(
        daemon.sync(main.path()),
        daemon.files(b_id),
        work_items(&daemon, b_id)
    );
    let a_sync = synced(&a_sync.expect("sync A"));
    assert_eq!((a_sync.created, a_sync.updated, a_sync.deleted), (1, 0, 0));
    assert!(b_files.1 && !b_files.0.contains(&"src/only_a.ts".to_owned()));
    assert!(b_work.contains("work only in B"));
    assert_eq!(generations(&secondary), b_before, "A never moves B");
    assert!(daemon.inspect(a_id, "onlyA").await.is_some());
    assert!(daemon.inspect(b_id, "onlyA").await.is_none());

    // And the other way round.
    let a_before = generations(main.path());
    fs::write(
        secondary.join("src/only_b.ts"),
        "export function onlyB(): number {\n  return 2;\n}\n",
    )
    .expect("edit B");
    let (b_sync, a_files, a_callers) = tokio::join!(
        daemon.sync(&secondary),
        daemon.files(a_id),
        daemon.callers(a_id, "helper")
    );
    assert_eq!(synced(&b_sync.expect("sync B")).created, 1);
    assert!(a_files.1 && !a_files.0.contains(&"src/only_b.ts".to_owned()));
    assert_eq!(a_callers, 1);
    assert_eq!(generations(main.path()), a_before, "B never moves A");

    // Two runtimes for five clients; no backend started or retried.
    assert_eq!(daemon.runtime.workspace_runtime_count(), 2);
    for workspace in [a_id, b_id] {
        let stats = daemon
            .runtime
            .semantic_stats(workspace)
            .await
            .expect("semantic");
        assert_eq!(stats.backend_starts, 0, "{stats:?}");
        assert!(stats.registered.is_empty(), "{stats:?}");
        assert!(
            stats.unavailable["typescript"].contains("no backend locator"),
            "{stats:?}"
        );
        let lifecycle = daemon
            .runtime
            .lifecycle_stats(workspace)
            .await
            .expect("runtime");
        assert!(lifecycle.watcher_attached);
        assert_eq!(lifecycle.watcher_attach_attempts, 1, "one watcher each");
    }
    daemon.stop().await;
}

// ======================================================= #66 upgrade

/// Every table's columns, sorted: the schema a migration list produces.
fn schema_shape(connection: &Connection) -> BTreeMap<String, Vec<String>> {
    let tables: Vec<String> = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .expect("tables")
        .query_map([], |row| row.get(0))
        .expect("tables")
        .collect::<Result<_, _>>()
        .expect("tables");
    tables
        .into_iter()
        .map(|table| {
            let mut columns: Vec<String> = connection
                .prepare(&format!("PRAGMA table_info(\"{table}\")"))
                .expect("table_info")
                .query_map([], |row| row.get(1))
                .expect("columns")
                .collect::<Result<_, _>>()
                .expect("columns");
            columns.sort();
            (table, columns)
        })
        .collect()
}

/// Turn the DB at `path` back into what this binary's *previous*
/// migration list wrote: undo the real last migration's tables and
/// columns and its ledger row. The result is checked against a fresh
/// N-1 database, so the fixture is exactly an older install's schema.
fn rewind_last_migration(path: &Path, kind: DbKind, migrations: &[Migration]) {
    let previous = &migrations[..migrations.len() - 1];
    let scratch = TestDir::create("shape");
    let shape = |list: &[Migration]| {
        let file = scratch.path().join(format!("v{}.db", list.len()));
        schema_shape(&db::open(&file, kind, list).expect("fresh").connection)
    };
    let (old, new) = (shape(previous), shape(migrations));
    let connection = Connection::open(path).expect("db");
    for (table, columns) in &new {
        match old.get(table) {
            None => connection
                .execute_batch(&format!("DROP TABLE \"{table}\""))
                .expect("drop table"),
            Some(kept) => {
                for column in columns.iter().filter(|column| !kept.contains(column)) {
                    connection
                        .execute_batch(&format!("ALTER TABLE \"{table}\" DROP COLUMN \"{column}\""))
                        .expect("drop column");
                }
            }
        }
    }
    let version = previous.last().expect("previous").version;
    connection
        .execute_batch(&format!(
            "DELETE FROM schema_migration WHERE version > {version};
             UPDATE db_meta SET schema_version = {version} WHERE id = 0;"
        ))
        .expect("ledger");
    assert_eq!(
        schema_shape(&connection),
        old,
        "a faithful N-1 {kind} schema"
    );
}

fn schema_version(db: &Path) -> (u32, u32) {
    let connection = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("db");
    connection
        .query_row(
            "SELECT (SELECT schema_version FROM db_meta WHERE id = 0), \
                    (SELECT MAX(version) FROM schema_migration)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("version")
}

fn doctor_schemas(response: &Response) -> Vec<(String, String)> {
    let Response::Doctor(doctor) = response else {
        panic!("expected doctor: {response:?}")
    };
    let DoctorWorkspaceWire::Initialized(workspace) = &doctor.workspace else {
        panic!("initialized: {doctor:?}")
    };
    std::iter::once(&doctor.global_db)
        .chain(&workspace.databases)
        .map(|check| (check.kind.clone(), format!("{:?}", check.state)))
        .collect()
}

fn index_identity(root: &Path) -> (Vec<u8>, Vec<(i64, String)>) {
    let index = WorkspacePaths::from_root(root).index_db;
    let incarnation = Connection::open_with_flags(&index, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("index.db")
        .query_row(
            "SELECT index_incarnation_uid FROM db_meta WHERE id = 0",
            [],
            |row| row.get(0),
        )
        .expect("incarnation");
    (incarnation, generations(root))
}

/// #66: a Workspace whose global / project / workspace databases are one
/// real migration behind is picked up by the current binary in place:
/// doctor reports it pending without writing, the first plain query
/// migrates and serves, identity / Project knowledge / Working State /
/// source survive, and the compatible index is not rebuilt.
#[tokio::test]
async fn an_older_schema_install_upgrades_in_place_without_a_rebuild() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let workspace = fixture_workspace("upgrade");
    let root = workspace.path();
    let paths = WorkspacePaths::from_root(root);
    let daemon = Daemon::start(&global, Some(deaf())).await;
    let Response::Install(_) = daemon.send(Request::Install(InstallRequest)).await else {
        panic!("install")
    };
    let init = daemon.init(root).await;
    let id: WorkspaceId = init.workspace_id.parse().expect("id");
    seed_policy(root, "i7-task4-kept");
    seed_work_item(root, id, "survive the upgrade");
    daemon.stop().await;

    let durable_before = durable(root);
    let source = snapshot(root);
    let index_before = index_identity(root);
    for (db, kind, migrations) in [
        (
            &global.global_db,
            DbKind::Global,
            schema::global::GLOBAL_MIGRATIONS,
        ),
        (
            &paths.project_db,
            DbKind::Project,
            schema::project::PROJECT_MIGRATIONS,
        ),
        (
            &paths.workspace_db,
            DbKind::Workspace,
            schema::workspace::WORKSPACE_MIGRATIONS,
        ),
    ] {
        rewind_last_migration(db, kind, migrations);
        let latest = migrations.last().expect("latest").version;
        assert_eq!(schema_version(db), (latest - 1, latest - 1));
    }

    // The current binary: doctor reports the Workspace's databases
    // pending and changes nothing.
    let daemon = Daemon::start(&global, Some(deaf())).await;
    let latest = |list: &[Migration]| list.last().expect("latest").version;
    let pending = doctor_schemas(
        &daemon
            .send(Request::Doctor(DoctorRequest { path: path(root) }))
            .await,
    );
    for kind in ["project", "workspace"] {
        let (_, state) = pending.iter().find(|(k, _)| k == kind).expect(kind);
        assert!(state.contains("MigrationPending"), "{kind}: {state}");
    }
    assert_eq!(
        schema_version(&paths.project_db).0,
        latest(schema::project::PROJECT_MIGRATIONS) - 1,
        "doctor never migrates"
    );

    // A repeated install over the existing one recreates nothing.
    let Response::Install(install) = daemon.send(Request::Install(InstallRequest)).await else {
        panic!("install")
    };
    assert!(
        !install.config_freshly_created && !install.db_freshly_created,
        "{install:?}"
    );

    // The first plain query activates, migrates and serves.
    assert!(
        work_items(&daemon, id)
            .await
            .contains("survive the upgrade")
    );
    assert!(rules(&daemon, id).await.contains("keep helper() pure"));
    for (db, list) in [
        (&global.global_db, schema::global::GLOBAL_MIGRATIONS),
        (&paths.project_db, schema::project::PROJECT_MIGRATIONS),
        (&paths.workspace_db, schema::workspace::WORKSPACE_MIGRATIONS),
    ] {
        assert_eq!(schema_version(db), (latest(list), latest(list)));
    }
    let current = doctor_schemas(
        &daemon
            .send(Request::Doctor(DoctorRequest { path: path(root) }))
            .await,
    );
    assert!(
        current.iter().all(|(_, state)| state.contains("Current")),
        "{current:?}"
    );

    // Identity and durable rows: the old rows are all still there.
    let durable_after = durable(root);
    for (table, rows) in &durable_before {
        if table.ends_with(":schema_migration") {
            continue;
        }
        let after = &durable_after[table];
        let ids = |rows: &[String]| -> Vec<String> {
            rows.iter()
                .map(|row| row.split(", ").take(2).collect::<Vec<_>>().join(", "))
                .collect()
        };
        assert_eq!(ids(after), ids(rows), "{table}");
    }
    let reinit = daemon.init(root).await;
    assert_eq!(
        (reinit.project_id, reinit.workspace_id),
        (init.project_id, init.workspace_id)
    );

    // A compatible index is reused, not rebuilt; source is untouched.
    assert_eq!(index_identity(root), index_before, "no rebuild");
    assert_eq!(snapshot(root), source);
    assert_eq!(daemon.callers(id, "helper").await, 1);
    let synced = synced(&daemon.sync(root).await.expect("sync"));
    assert_eq!((synced.created, synced.updated, synced.deleted), (0, 0, 0));
    daemon.stop().await;
}

/// #66: a durable migration that fails rolls back: no ledger row, no
/// version bump, the Project's knowledge and the blocking state kept,
/// the Workspace refused rather than served, doctor still pending -- and
/// once the cause is gone the next open migrates normally.
#[tokio::test]
async fn a_failed_durable_migration_records_nothing_and_keeps_the_rows() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let workspace = fixture_workspace("failed-upgrade");
    let root = workspace.path();
    let paths = WorkspacePaths::from_root(root);
    let daemon = Daemon::start(&global, Some(deaf())).await;
    let id = daemon.init_id(root).await;
    seed_policy(root, "i7-task4-failure");
    daemon.stop().await;

    let migrations = schema::project::PROJECT_MIGRATIONS;
    let latest = migrations.last().expect("latest").version;
    rewind_last_migration(&paths.project_db, DbKind::Project, migrations);
    // The fault: an object already holding the name the real pending
    // migration creates, so its CREATE TABLE fails inside the runner.
    Connection::open(&paths.project_db)
        .expect("project.db")
        .execute_batch("CREATE TABLE knowledge_promotion (blocker INTEGER); INSERT INTO knowledge_promotion VALUES (1);")
        .expect("blocker");
    let before = rows(&paths.project_db);

    let daemon = Daemon::start(&global, Some(deaf())).await;
    let outcome = daemon
        .query_outcome(
            WorkspaceSelectorWire::Id { workspace_id: id },
            QueryOperationWire::Knowledge(KnowledgeWire::WorkItems {
                statuses: vec![WorkItemStatusWire::Open],
                limit: NonZeroUsize::new(10).expect("nz"),
            }),
        )
        .await;
    assert!(matches!(outcome, QueryOutcomeWire::Err(_)), "{outcome:?}");
    let Response::Error(error) = daemon
        .send(Request::Init(InitRequest { path: path(root) }))
        .await
    else {
        panic!("a failed migration is never a successful init")
    };
    // #13 §9: the driver detail stays in the daemon log; the client gets
    // the typed internal kind, and doctor says what is pending.
    assert_eq!(error.kind, ErrorKind::DaemonInternal, "{}", error.message);

    assert_eq!(schema_version(&paths.project_db), (latest - 1, latest - 1));
    assert_eq!(
        rows(&paths.project_db),
        before,
        "rolled back, nothing reset"
    );
    let pending = doctor_schemas(
        &daemon
            .send(Request::Doctor(DoctorRequest { path: path(root) }))
            .await,
    );
    let (_, state) = pending
        .iter()
        .find(|(k, _)| k == "project")
        .expect("project");
    assert!(state.contains("MigrationPending"), "{state}");

    // The cause removed, the same binary migrates and serves.
    Connection::open(&paths.project_db)
        .expect("project.db")
        .execute_batch("DROP TABLE knowledge_promotion")
        .expect("unblock");
    daemon.init(root).await;
    assert_eq!(schema_version(&paths.project_db), (latest, latest));
    assert!(rules(&daemon, id).await.contains("keep helper() pure"));
    daemon.stop().await;
}

// ======================================================= #70 status

impl Daemon {
    async fn status(&self, path: Option<&Path>) -> StatusResponse {
        match self
            .send(Request::Status(StatusRequest {
                path: path.map(self::path),
            }))
            .await
        {
            Response::Status(status) => status,
            other => panic!("status failed: {other:?}"),
        }
    }
}

fn report(status: &StatusResponse) -> &WorkspaceStatusReportWire {
    match status.workspace.as_ref().expect("a path-scoped status") {
        WorkspaceStatusWire::Initialized(report) => report,
        other => panic!("not initialized: {other:?}"),
    }
}

/// The STABLE basis as `index.db` stores it, read independently.
fn stored(root: &Path) -> IndexBasisWire {
    let store = GenerationStore::open(&WorkspacePaths::from_root(root).index_db).expect("index");
    let stable = store.current_stable().expect("stable").expect("published");
    IndexBasisWire {
        index_incarnation: store.index_incarnation_id().expect("incarnation"),
        workspace_revision: store
            .current_workspace_revision()
            .expect("clock")
            .expect("revision"),
        generation_no: stable.generation_no,
        generation_basis_revision: stable.basis_workspace_revision,
    }
}

/// #70: `status <path>` reports the stored basis as stored, claims
/// Current only while an active runtime with an attached watcher holds
/// it, and is read-only: no reconcile, no activation, no backend start.
#[tokio::test]
async fn status_reports_the_stored_basis_and_claims_currentness_only_when_held() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let workspace = fixture_workspace("status");
    let root = workspace.path();
    let daemon = Daemon::start(&global, Some(deaf())).await;

    // Daemon-only status: unchanged, no Workspace part.
    let plain = daemon.status(None).await;
    assert_eq!(plain.protocol_version, PROTOCOL_VERSION);
    assert!(plain.workspace.is_none());

    // Active, watcher attached, proven current at init.
    let init = daemon.init(root).await;
    let id: WorkspaceId = init.workspace_id.parse().expect("id");
    let active = daemon.status(Some(root)).await;
    let active = report(&active);
    assert_eq!(
        (&active.project_id, &active.workspace_id),
        (&init.project_id, &init.workspace_id)
    );
    assert_eq!(active.basis, StoredBasisWire::Stable(stored(root)));
    assert_eq!(active.index, IndexCheckWire::Current);
    assert_eq!(
        active.runtime,
        RuntimeCheckWire::Active {
            watcher: WatcherCheckWire::Attached
        }
    );
    assert!(
        active
            .semantic
            .iter()
            .any(|backend| backend.family == "typescript")
    );

    // Read-only: a change on disk the deaf watcher never reports stays
    // unreconciled however often status runs.
    fs::write(root.join("src/late.ts"), "export const late = 1;\n").expect("edit");
    let (gens, rows, source) = (generations(root), durable(root), snapshot(root));
    for _ in 0..3 {
        daemon.status(Some(root)).await;
    }
    assert_eq!(generations(root), gens, "status never publishes");
    assert_eq!(durable(root), rows);
    assert_eq!(snapshot(root), source);
    let semantic = daemon.runtime.semantic_stats(id).await.expect("semantic");
    assert_eq!(semantic.backend_starts, 0);

    // Dirty: the index says so, and status repeats it without settling.
    Connection::open(WorkspacePaths::from_root(root).index_db)
        .expect("index.db")
        .execute(
            "UPDATE component_state SET freshness_state = 'DIRTY' \
             WHERE component_kind = 'RESOURCE_INDEX'",
            [],
        )
        .expect("dirty");
    let dirty = daemon.status(Some(root)).await;
    assert!(
        matches!(report(&dirty).index, IndexCheckWire::NotCurrent { .. }),
        "{:?}",
        report(&dirty).index
    );
    assert_eq!(generations(root), gens);

    // Sync: the status basis is the sync's own `after`.
    let response = daemon.sync(root).await.expect("sync");
    let PostCommandRefreshWire::Current { after, .. } = &response.refresh else {
        panic!("sync is Current")
    };
    let synced = daemon.status(Some(root)).await;
    assert_eq!(
        report(&synced).basis,
        StoredBasisWire::Stable(after.clone())
    );
    assert_eq!(report(&synced).index, IndexCheckWire::Current);
    daemon.stop().await;

    // A new daemon holds no runtime: the stored basis is still reported,
    // currentness is not, and status activates nothing.
    let daemon = Daemon::start(&global, Some(deaf())).await;
    let inactive = daemon.status(Some(root)).await;
    let inactive = report(&inactive);
    assert_eq!(inactive.basis, StoredBasisWire::Stable(after.clone()));
    assert!(matches!(inactive.index, IndexCheckWire::NotMeasured { .. }));
    assert_eq!(inactive.runtime, RuntimeCheckWire::Inactive);
    assert_eq!(daemon.runtime.workspace_runtime_count(), 0);

    // Detached and never-initialized are typed, as in doctor.
    daemon.uninit(root).await.expect("uninit");
    let detached = daemon.status(Some(root)).await;
    let Some(WorkspaceStatusWire::NotInitialized { reason, .. }) = &detached.workspace else {
        panic!("{detached:?}")
    };
    assert!(reason.contains("WorkspaceDetached"), "{reason}");
    let elsewhere = TestDir::create("status-none");
    let none = daemon.status(Some(elsewhere.path())).await;
    assert!(matches!(
        none.workspace,
        Some(WorkspaceStatusWire::NotInitialized { .. })
    ));
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

    // #70: `status` alone is the daemon's; `status <path>` adds the
    // Workspace's stored basis -- the sync's own `after` -- and the
    // runtime's currentness, in the same line style.
    let plain = run(vec!["status".into()]).await.expect("cli");
    let plain = String::from_utf8_lossy(&plain.stdout).into_owned();
    assert!(
        plain.starts_with("brainprintd ") && !plain.contains("workspace:"),
        "{plain}"
    );
    let scoped = run(vec!["status".into(), ws.clone()]).await.expect("cli");
    let scoped_text = String::from_utf8_lossy(&scoped.stdout).into_owned();
    assert_eq!(scoped.status.code(), Some(0), "{scoped_text}");
    let after = stored(workspace.path());
    assert!(
        has(&scoped_text, &["revision:", &after.workspace_revision]),
        "{scoped_text}"
    );
    let generation = after.generation_no.to_string();
    assert!(
        has(
            &scoped_text,
            &[
                "generation:",
                &generation,
                "(basis",
                "revision",
                &format!("{})", after.generation_basis_revision)
            ]
        ),
        "{scoped_text}"
    );
    assert!(has(&scoped_text, &["index:", "current"]), "{scoped_text}");
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
