//! #38 (P0 correction) product runtime integration acceptance.
//!
//! Every Workspace state here is produced by the daemon itself -- `Init`
//! over the real local IPC, the daemon-owned Workspace runtime, its
//! watcher, `TargetedRefresh` and `Reconcile`. No test calls an engine
//! indexer directly (acceptance 22, asserted below). The index is only
//! *read* (rusqlite, read-only) to verify factual state: revision,
//! generations, resource identities, schema versions.
//!
//! Deterministic mutation/loss/bulk cases drive a scripted watch source
//! through `Server::bind_with_watch_factory`; realism cases use the
//! production `notify` watcher via `Server::bind`.

use std::{
    collections::BTreeSet,
    env, fs,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process::{self, Command},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use brainprint_core::{
    PROTOCOL_VERSION, WorkspaceId,
    protocol::{
        self, ClientConnection, HandshakeRequest, InitRequest, InitResponse, Request, Response,
        query::*,
    },
};
use brainprint_daemon::{
    query::{
        DaemonQueryRuntime,
        lifecycle::{LifecycleStats, WatchFactory},
    },
    runtime_paths,
    server::Server,
};
use brainprint_engine::{paths::GlobalPaths, watch::RawWatchEvent, watch::WatchSource};
use rusqlite::{Connection, OpenFlags};

// ------------------------------------------------------------- harness

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let path = env::temp_dir().join(format!(
            "bp-p038-{label}-{}-{}",
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

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    fs::write(path, body).expect("write");
}

fn fixture_workspace(label: &str, git_repo: bool) -> TestDir {
    let workspace = TestDir::create(label);
    write(workspace.path(), "src/shared.ts", SHARED_TS);
    write(workspace.path(), "src/app.ts", APP_TS);
    if git_repo {
        git(workspace.path(), &["init", "-q", "."]);
    }
    workspace
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

/// `Path::canonicalize()` on Windows returns a `\\?\`-prefixed verbatim
/// path; git.exe (MSYS) does not accept that prefix in a path argument
/// (only `Command::current_dir` tolerates it). Strip it for arguments.
fn git_path_arg(path: &Path) -> String {
    let raw = path.to_string_lossy().into_owned();
    match raw.strip_prefix(r"\\?\") {
        Some(stripped) => stripped.to_owned(),
        None => raw,
    }
}

/// A watch source the test drives: it "observes" exactly the events the
/// test pushes, like an OS watcher whose events arrive on demand.
#[derive(Clone, Default)]
struct Script(Arc<Mutex<Vec<RawWatchEvent>>>);

impl Script {
    fn push(&self, event: RawWatchEvent) {
        self.0.lock().expect("script").push(event);
    }
}

struct ScriptedSource(Script);

impl WatchSource for ScriptedSource {
    fn drain(&mut self) -> Vec<RawWatchEvent> {
        std::mem::take(&mut *self.0.0.lock().expect("script"))
    }
}

#[derive(Clone, Copy)]
enum Watcher {
    Scripted,
    Unavailable,
}

fn factory(kind: Watcher, script: Script, attempts: Arc<AtomicUsize>) -> WatchFactory {
    Arc::new(move |_root: &Path| {
        attempts.fetch_add(1, Ordering::Relaxed);
        match kind {
            Watcher::Scripted => {
                Ok(Box::new(ScriptedSource(script.clone())) as Box<dyn WatchSource>)
            }
            Watcher::Unavailable => Err("watcher backend unavailable (fixture)".to_owned()),
        }
    })
}

struct Daemon {
    handle: tokio::task::JoinHandle<()>,
    endpoint: runtime_paths::RuntimeEndpoint,
    runtime: Arc<DaemonQueryRuntime>,
}

impl Daemon {
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
        // Let the dropped runtime's worker threads observe disconnection.
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
                client_kind: "p0-38".to_owned(),
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

    async fn stats(&self, workspace: WorkspaceId) -> LifecycleStats {
        self.runtime
            .lifecycle_stats(workspace)
            .await
            .expect("workspace runtime exists")
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
            request_id: "5f0c2a4e-1b7d-4c8e-9a6f-0e1d2c3b4a59".to_owned(),
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

/// `find files` over the whole Workspace: (file paths, index current).
async fn files(daemon: &Daemon, workspace: WorkspaceId) -> (BTreeSet<String>, bool) {
    files_via(daemon.connect().await, workspace).await
}

async fn files_via(
    mut connection: ClientConnection,
    workspace: WorkspaceId,
) -> (BTreeSet<String>, bool) {
    let result = query(
        &mut connection,
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
    (
        listing
            .entries
            .into_iter()
            .map(|entry| entry.path_rel)
            .collect(),
        listing.currentness == CurrentnessWire::Current,
    )
}

/// `inspect` a symbol: the delivered current source, when resolved.
async fn inspect(daemon: &Daemon, workspace: WorkspaceId, name: &str) -> Option<String> {
    let mut connection = daemon.connect().await;
    let result = query(
        &mut connection,
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
        DeliveredItemWire::Full(EvidenceWire::CurrentSource(range)) => Some(range.source.clone()),
        _ => None,
    })
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

fn index_db(root: &Path) -> Connection {
    Connection::open_with_flags(
        root.join(".brainprint/data/index.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("index.db")
}

fn clock(root: &Path) -> (String, String) {
    index_db(root)
        .query_row(
            "SELECT current_workspace_revision, watcher_continuity_state FROM workspace_clock WHERE id = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("clock")
}

fn generations(root: &Path) -> Vec<(i64, String, String)> {
    let connection = index_db(root);
    let mut statement = connection
        .prepare(
            "SELECT generation_no, state, basis_workspace_revision FROM generation ORDER BY id",
        )
        .expect("prepare");
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows")
}

fn count(root: &Path, table: &str) -> i64 {
    index_db(root)
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count")
}

/// ACTIVE file Resource identities by path.
fn resource_ids(root: &Path) -> Vec<(String, Vec<u8>)> {
    let connection = index_db(root);
    let mut statement = connection
        .prepare("SELECT path_rel, uid FROM resource WHERE state = 'ACTIVE' ORDER BY path_rel")
        .expect("prepare");
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows")
}

fn scripted(kind: Watcher) -> (Script, Arc<AtomicUsize>, WatchFactory) {
    let script = Script::default();
    let attempts = Arc::new(AtomicUsize::new(0));
    let factory = factory(kind, script.clone(), Arc::clone(&attempts));
    (script, attempts, factory)
}

// ======================================================= fresh product path

async fn assert_structurally_ready(git_repo: bool) {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global, None).await;
    let workspace = fixture_workspace("fresh", git_repo);

    let started = Instant::now();
    let id = daemon.init(workspace.path()).await;
    let init_ms = started.elapsed().as_millis();

    // 3: a STABLE generation exists the moment init returns, at the
    // locked initial revision "0".
    let gens = generations(workspace.path());
    assert_eq!(gens, vec![(1, "STABLE".to_owned(), "0".to_owned())]);
    assert_eq!(
        clock(workspace.path()),
        ("0".to_owned(), "CONTINUOUS".to_owned())
    );
    // 4: Resources and supported Symbols published.
    assert!(count(workspace.path(), "resource") > 0);
    assert!(count(workspace.path(), "symbol") >= 2);
    // 5: structured files + inspect succeed without any test helper.
    let (listed, current) = files(&daemon, id).await;
    assert!(current);
    assert_eq!(
        listed,
        BTreeSet::from(["src/app.ts".to_owned(), "src/shared.ts".to_owned()])
    );
    assert!(
        inspect(&daemon, id, "run")
            .await
            .expect("run resolves")
            .contains("helper()")
    );

    let stats = daemon.stats(id).await;
    assert_eq!((stats.baseline_scans, stats.watcher_attached), (1, true));
    println!(
        "MEASURE fresh_init git={git_repo} init_ms={init_ms} resources={} symbols={}",
        count(workspace.path(), "resource"),
        count(workspace.path(), "symbol")
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a01_a05_fresh_git_init_is_structurally_ready() {
    assert_structurally_ready(true).await;
}

#[tokio::test]
async fn a06_fresh_non_git_init_is_structurally_ready() {
    assert_structurally_ready(false).await;
}

#[tokio::test]
async fn reinit_keeps_identities_and_does_not_rebaseline() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let (_script, _attempts, factory) = scripted(Watcher::Scripted);
    let daemon = Daemon::start(&global, Some(factory)).await;
    let workspace = fixture_workspace("reinit", true);
    let id = daemon.init(workspace.path()).await;
    let before = (
        generations(workspace.path()),
        resource_ids(workspace.path()),
    );
    assert_eq!(daemon.init(workspace.path()).await, id);
    assert_eq!(
        (
            generations(workspace.path()),
            resource_ids(workspace.path())
        ),
        before
    );
    assert_eq!(daemon.stats(id).await.baseline_scans, 1);
    daemon.stop().await;
}

// ================================================================= mutation

#[tokio::test]
async fn a07_single_file_modify_takes_the_targeted_fast_path() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let (script, _attempts, factory) = scripted(Watcher::Scripted);
    let daemon = Daemon::start(&global, Some(factory)).await;
    let workspace = fixture_workspace("modify", true);
    let root = workspace.path().to_path_buf();
    let id = daemon.init(&root).await;
    let before = daemon.stats(id).await;

    write(
        &root,
        "src/app.ts",
        &format!("{APP_TS}\nexport function runTwice(): number {{\n  return run() + run();\n}}\n"),
    );
    script.push(RawWatchEvent::Modified {
        path: root.join("src/app.ts"),
    });
    script.push(RawWatchEvent::Modified {
        path: root.join("src/app.ts"),
    });

    let source = inspect(&daemon, id, "runTwice")
        .await
        .expect("new symbol current");
    assert!(source.contains("run() + run()"));
    let after = daemon.stats(id).await;
    assert_eq!(
        after.journal_entries_ingested - before.journal_entries_ingested,
        2,
        "event -> DIRTY journal evidence"
    );
    assert_eq!(
        after.refresh_publications - before.refresh_publications,
        1,
        "one targeted refresh publication"
    );
    assert_eq!(
        after.reconciles, before.reconciles,
        "no full Workspace reconcile on the eligible fast path"
    );
    assert_eq!(clock(&root).0, "1", "first confirmed change -> revision 1");
    assert_eq!(
        generations(&root)
            .last()
            .map(|last| (last.0, last.1.clone())),
        Some((2, "STABLE".to_owned()))
    );
    daemon.stop().await;
}

/// The production `notify` watcher end to end. Which path a real save
/// takes depends on the events the platform reports (e.g. macOS FSEvents
/// can flag a modify of a recently created file as a CREATE, which the
/// fast path correctly refuses); correctness must hold either way.
#[tokio::test]
async fn a07_real_watcher_save_reaches_current_truth() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global, None).await;
    let workspace = fixture_workspace("realsave", true);
    let id = daemon.init(workspace.path()).await;
    let before = daemon.stats(id).await;

    write(
        workspace.path(),
        "src/app.ts",
        &format!("{APP_TS}\nexport function runTwice(): number {{\n  return run() + run();\n}}\n"),
    );
    eventually("runTwice becomes current", || async {
        inspect(&daemon, id, "runTwice").await.is_some()
    })
    .await;
    let after = daemon.stats(id).await;
    assert!(
        after.journal_entries_ingested > before.journal_entries_ingested,
        "real watcher events journaled"
    );
    assert!(
        after.refresh_publications + after.reconcile_publications
            > before.refresh_publications + before.reconcile_publications
    );
    println!(
        "MEASURE real_save journal={} refresh_pub={} refresh_deferred={} reconciles={}",
        after.journal_entries_ingested - before.journal_entries_ingested,
        after.refresh_publications - before.refresh_publications,
        after.refresh_deferrals - before.refresh_deferrals,
        after.reconciles - before.reconciles
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a08_create_delete_move_reconcile_to_current_truth() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let (script, _attempts, factory) = scripted(Watcher::Scripted);
    let daemon = Daemon::start(&global, Some(factory)).await;
    let workspace = fixture_workspace("cdm", true);
    let root = workspace.path().to_path_buf();
    let id = daemon.init(&root).await;
    let app_id = resource_ids(&root)
        .into_iter()
        .find(|(path, _)| path == "src/app.ts")
        .expect("app")
        .1;
    let before = daemon.stats(id).await;

    write(
        &root,
        "src/extra.ts",
        "export function extra(): number {\n  return 1;\n}\n",
    );
    script.push(RawWatchEvent::Created {
        path: root.join("src/extra.ts"),
    });
    fs::remove_file(root.join("src/shared.ts")).expect("delete");
    script.push(RawWatchEvent::Removed {
        path: root.join("src/shared.ts"),
    });
    fs::rename(root.join("src/app.ts"), root.join("src/main.ts")).expect("move");
    script.push(RawWatchEvent::RenamedPair {
        from: root.join("src/app.ts"),
        to: root.join("src/main.ts"),
    });

    let (listed, current) = files(&daemon, id).await;
    assert!(current);
    assert_eq!(
        listed,
        BTreeSet::from(["src/extra.ts".to_owned(), "src/main.ts".to_owned()])
    );
    assert!(
        inspect(&daemon, id, "extra").await.is_some(),
        "created symbol current"
    );
    assert_eq!(
        inspect(&daemon, id, "helper").await,
        None,
        "deleted symbol not current"
    );
    assert!(
        inspect(&daemon, id, "run").await.is_some(),
        "moved symbol current at its new path"
    );
    let main_id = resource_ids(&root)
        .into_iter()
        .find(|(path, _)| path == "src/main.ts")
        .expect("main")
        .1;
    assert_eq!(
        main_id, app_id,
        "a backend-vouched paired rename keeps the identity"
    );
    let after = daemon.stats(id).await;
    assert!(after.reconciles > before.reconciles);
    daemon.stop().await;
}

#[tokio::test]
async fn a09_bulk_change_is_one_reconcile_not_a_storm() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let (script, _attempts, factory) = scripted(Watcher::Scripted);
    let daemon = Daemon::start(&global, Some(factory)).await;
    let workspace = fixture_workspace("bulk", false);
    let root = workspace.path().to_path_buf();
    let id = daemon.init(&root).await;
    let before = daemon.stats(id).await;

    for index in 0..6 {
        let rel = format!("src/bulk{index}.ts");
        write(
            &root,
            &rel,
            &format!("export function bulk{index}(): number {{\n  return {index};\n}}\n"),
        );
        script.push(RawWatchEvent::Created {
            path: root.join(&rel),
        });
        script.push(RawWatchEvent::Modified {
            path: root.join(&rel),
        });
    }
    write(&root, "src/app.ts", &format!("{APP_TS}// touched\n"));
    script.push(RawWatchEvent::Modified {
        path: root.join("src/app.ts"),
    });

    let (listed, current) = files(&daemon, id).await;
    assert!(current);
    assert_eq!(listed.len(), 8);
    let after = daemon.stats(id).await;
    let reconciles = after.reconciles - before.reconciles;
    assert!(
        reconciles <= 2,
        "one coalesced correctness path, not one per event ({reconciles} reconciles for 13 events)"
    );
    assert!(
        after.refresh_publications == before.refresh_publications,
        "multi-Resource batch never takes the single-file path"
    );
    println!(
        "MEASURE bulk events=13 reconciles={reconciles} ingested={}",
        after.journal_entries_ingested - before.journal_entries_ingested
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a10_a11_continuity_loss_and_mtime_preserving_edit_are_recovered_by_verified_reconcile() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let (script, attempts, factory) = scripted(Watcher::Scripted);
    let daemon = Daemon::start(&global, Some(factory)).await;
    let workspace = fixture_workspace("loss", true);
    let root = workspace.path().to_path_buf();
    let id = daemon.init(&root).await;
    assert!(
        inspect(&daemon, id, "helper")
            .await
            .expect("helper")
            .contains("42")
    );

    // 11: same size, mtime restored, and the watcher never reports it.
    let path = root.join("src/shared.ts");
    let mtime = fs::metadata(&path)
        .expect("meta")
        .modified()
        .expect("mtime");
    let edited = SHARED_TS.replace("42", "24");
    assert_eq!(edited.len(), SHARED_TS.len());
    fs::write(&path, &edited).expect("write");
    fs::File::options()
        .write(true)
        .open(&path)
        .expect("open")
        .set_modified(mtime)
        .expect("restore mtime");
    assert_eq!(
        fs::metadata(&path)
            .expect("meta")
            .modified()
            .expect("mtime"),
        mtime
    );

    // 10: the stream reports it can no longer be trusted.
    let before = daemon.stats(id).await;
    script.push(RawWatchEvent::ContinuityLost {
        detail: "fixture overflow".to_owned(),
    });
    let source = inspect(&daemon, id, "helper")
        .await
        .expect("helper current after recovery");
    assert!(
        source.contains("24"),
        "the verified reconcile found the mtime-preserving edit: {source}"
    );
    let after = daemon.stats(id).await;
    assert!(
        after.reconciles > before.reconciles,
        "continuity loss -> reconcile"
    );
    assert!(
        attempts.load(Ordering::Relaxed) >= 2,
        "watcher re-attached before the reconcile"
    );
    assert_eq!(clock(&root).1, "CONTINUOUS");
    daemon.stop().await;
}

// =========================================================== restart/degraded

#[tokio::test]
async fn a12_a14_restart_reconciles_offline_edits_and_keeps_unchanged_identities() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let workspace = fixture_workspace("restart", true);
    let root = workspace.path().to_path_buf();

    let (_s1, _a1, first_factory) = scripted(Watcher::Scripted);
    let first = Daemon::start(&global, Some(first_factory)).await;
    let id = first.init(&root).await;
    first.stop().await;

    // 14: an unchanged restart republishes nothing and keeps identities.
    let identities = resource_ids(&root);
    let gens = generations(&root);
    let (_s2, _a2, second_factory) = scripted(Watcher::Scripted);
    let second = Daemon::start(&global, Some(second_factory)).await;
    assert!(inspect(&second, id, "run").await.is_some());
    let stats = second.stats(id).await;
    assert_eq!(
        (
            stats.reconciles,
            stats.reconcile_publications,
            stats.baseline_scans
        ),
        (1, 0, 0)
    );
    assert_eq!(
        (resource_ids(&root), generations(&root)),
        (identities.clone(), gens.clone())
    );
    second.stop().await;

    // 12: edit while the daemon is down; the first query after restart
    // must reconcile before serving -- never the pre-restart snapshot.
    write(
        &root,
        "src/shared.ts",
        &format!("{SHARED_TS}\nexport function offline(): number {{\n  return 7;\n}}\n"),
    );
    let (_s3, _a3, third_factory) = scripted(Watcher::Scripted);
    let third = Daemon::start(&global, Some(third_factory)).await;
    assert!(
        inspect(&third, id, "offline").await.is_some(),
        "offline edit visible on the first query"
    );
    let stats = third.stats(id).await;
    assert_eq!((stats.reconciles, stats.reconcile_publications), (1, 1));
    assert_eq!(generations(&root).len(), gens.len() + 1);
    let shared = |ids: &[(String, Vec<u8>)]| {
        ids.iter()
            .find(|(path, _)| path == "src/shared.ts")
            .map(|(_, id)| id.clone())
    };
    assert_eq!(
        shared(&resource_ids(&root)),
        shared(&identities),
        "a modify keeps the identity"
    );
    third.stop().await;
}

#[tokio::test]
async fn a13_watcher_unavailable_degrades_to_query_time_reconcile() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let (_script, _attempts, factory) = scripted(Watcher::Unavailable);
    let daemon = Daemon::start(&global, Some(factory)).await;
    let workspace = fixture_workspace("nowatch", true);
    let root = workspace.path().to_path_buf();
    let id = daemon.init(&root).await;

    let stats = daemon.stats(id).await;
    assert!(!stats.watcher_attached);
    assert!(
        stats
            .watcher_error
            .as_deref()
            .is_some_and(|error| error.contains("unavailable"))
    );
    assert_ne!(
        clock(&root).1,
        "CONTINUOUS",
        "no false continuous-READY claim"
    );

    write(
        &root,
        "src/app.ts",
        &format!("{APP_TS}\nexport function late(): number {{\n  return 3;\n}}\n"),
    );
    assert!(
        inspect(&daemon, id, "late").await.is_some(),
        "query-time verified reconcile saw the edit"
    );
    let before = daemon.stats(id).await.reconciles;
    let _ = files(&daemon, id).await;
    assert_eq!(
        daemon.stats(id).await.reconciles,
        before + 1,
        "every current-dependent query reconciles in degraded mode"
    );
    daemon.stop().await;
}

// ======================================================= concurrency/isolation

#[tokio::test]
async fn a15_two_clients_share_one_runtime_and_one_watcher() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let (_script, attempts, factory) = scripted(Watcher::Scripted);
    let daemon = Daemon::start(&global, Some(factory)).await;
    let workspace = fixture_workspace("clients", true);
    let id = daemon.init(workspace.path()).await;
    let (a, b) = tokio::join!(inspect(&daemon, id, "run"), files(&daemon, id));
    assert!(a.is_some() && b.1);
    for _ in 0..3 {
        let _ = files(&daemon, id).await;
    }
    assert_eq!(daemon.runtime.workspace_runtime_count(), 1);
    assert_eq!(
        attempts.load(Ordering::Relaxed),
        1,
        "one watcher for all clients"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a16_two_worktrees_have_separate_runtime_and_currentness() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global, None).await;
    let main = fixture_workspace("wt-main", true);
    git(main.path(), &["add", "."]);
    git(main.path(), &["commit", "-q", "-m", "fixture"]);
    let linked = TestDir::create("wt-linked");
    fs::remove_dir(linked.path()).expect("empty dir");
    git(
        main.path(),
        &[
            "worktree",
            "add",
            "-q",
            &git_path_arg(linked.path()),
            "HEAD",
        ],
    );

    let main_id = daemon.init(main.path()).await;
    let linked_id = daemon.init(linked.path()).await;
    assert_ne!(main_id, linked_id);
    assert_eq!(daemon.runtime.workspace_runtime_count(), 2);
    let linked_before = (daemon.stats(linked_id).await, generations(linked.path()));

    write(
        main.path(),
        "src/app.ts",
        &format!("{APP_TS}\nexport function onlyMain(): number {{\n  return 1;\n}}\n"),
    );
    eventually("main sees its own edit", || async {
        inspect(&daemon, main_id, "onlyMain").await.is_some()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert_eq!(
        inspect(&daemon, linked_id, "onlyMain").await,
        None,
        "no cross-worktree leakage"
    );
    let linked_after = daemon.stats(linked_id).await;
    assert_eq!(
        linked_after.journal_entries_ingested, linked_before.0.journal_entries_ingested,
        "an event in A never dirties B"
    );
    assert_eq!(generations(linked.path()), linked_before.1);
    assert_eq!(clock(linked.path()).0, "0");
    daemon.stop().await;
}

#[tokio::test]
async fn a17_queries_never_observe_a_half_published_generation() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let (script, _attempts, factory) = scripted(Watcher::Scripted);
    let daemon = Daemon::start(&global, Some(factory)).await;
    let workspace = fixture_workspace("atomic", false);
    let root = workspace.path().to_path_buf();
    let id = daemon.init(&root).await;

    for round in 0..4 {
        let names: Vec<String> = (0..10)
            .map(|index| format!("src/r{round}_{index}.ts"))
            .collect();
        for name in &names {
            write(&root, name, "export const x = 1;\n");
            script.push(RawWatchEvent::Created {
                path: root.join(name),
            });
        }
        let mut expected: BTreeSet<String> = files(&daemon, id).await.0;
        expected.extend(names.iter().cloned());
        // Many clients at once while the publication is in flight.
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let connection = daemon.connect().await;
            tasks.push(tokio::spawn(files_via(connection, id)));
        }
        for task in tasks {
            let (listed, current) = task.await.expect("client task");
            assert!(current);
            assert_eq!(
                listed, expected,
                "round {round}: every client sees the complete publication"
            );
        }
        let building = index_db(&root)
            .query_row(
                "SELECT count(*) FROM generation WHERE state = 'BUILDING'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count");
        assert_eq!(building, 0);
    }
    daemon.stop().await;
}

// ================================================================ boundary

#[tokio::test]
async fn a21_a23_no_semantic_start_and_schema_versions_unchanged() {
    let home = TestDir::create("home");
    let global = GlobalPaths::from_home(home.path());
    let daemon = Daemon::start(&global, None).await;
    let workspace = fixture_workspace("schema", true);
    let _ = daemon.init(workspace.path()).await;
    let version = |db: PathBuf| -> i64 {
        Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("db")
            .query_row(
                "SELECT schema_version FROM db_meta WHERE id = 0",
                [],
                |row| row.get(0),
            )
            .expect("schema_version")
    };
    let data = workspace.path().join(".brainprint/data");
    assert_eq!(version(global.global_db.clone()), 4);
    assert_eq!(version(data.join("project.db")), 4);
    assert_eq!(version(data.join("workspace.db")), 7);
    assert_eq!(version(data.join("index.db")), 10);
    daemon.stop().await;

    // The structural lifecycle constructs no semantic runtime or launcher.
    // Lazy semantic activation lives only in `query/semantic.rs` (#39),
    // whose fresh-init/structural start count is measured factually by
    // `p0_39_semantic_runtime_acceptance`.
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for file in [
        "query/lifecycle.rs",
        "query/runtime.rs",
        "server.rs",
        "handlers.rs",
    ] {
        let text = fs::read_to_string(src.join(file)).expect("src");
        for forbidden in ["SemanticRuntimeSupervisor", "_semantic::", "Launcher"] {
            assert!(
                !text.contains(forbidden),
                "{file} must not start semantic backends ({forbidden})"
            );
        }
    }
}

#[test]
fn a22_no_test_side_indexer_in_this_suite() {
    let text = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/p0_38_runtime_integration_acceptance.rs"),
    )
    .expect("self");
    for forbidden in [
        concat!("Baseline", "Scan"),
        concat!("run_initial", "_scan"),
        concat!("Targeted", "Refresh::"),
        concat!("Reconcile", "::open"),
    ] {
        assert!(
            !text.contains(forbidden),
            "acceptance must not manufacture product state via {forbidden}"
        );
    }
    let _ = SystemTime::now();
}
