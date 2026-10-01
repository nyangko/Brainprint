//! #55 step 1 acceptance: the pre-command baseline and the forced
//! post-command verified refresh on a Workspace worker, with the net
//! Resource delta between them -- through the daemon's own runtime
//! (`DaemonQueryRuntime::command_baseline` / `post_command_refresh`)
//! against a real in-process daemon and Workspace. Not yet wired to any
//! verification path.
//!
//! The watch source is scripted: it delivers exactly the events a test
//! pushes, so "the watcher saw nothing" is a fact, not a timing. The index
//! is only read (rusqlite, read-only) to check the reported basis.

use std::{
    collections::{BTreeSet, HashMap},
    env, fs,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process::{self, Command},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use brainprint_core::{
    IndexIncarnationId, PROTOCOL_VERSION, ResourceId, WorkspaceId,
    protocol::{
        self, ClientConnection, HandshakeRequest, InitRequest, InitResponse, Request, Response,
        query::*,
    },
};
use brainprint_daemon::{
    query::{
        DaemonQueryRuntime,
        lifecycle::{
            CommandBaseline, CommandBasisError, IndexBasis, LifecycleStats,
            PostCommandRefreshReport, ResourceDeltaKind, WatchFactory,
        },
    },
    runtime_paths,
    server::Server,
};
use brainprint_engine::{
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
            "bp-i6t6-{label}-{}-{}",
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
const EXTRA_TS: &str = "export const extra = 1;\n";

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    fs::write(path, body).expect("write");
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git");
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A watch source the test drives: it "observes" exactly what is pushed.
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

struct Fixture {
    _home: TestDir,
    roots: Vec<TestDir>,
    script: Script,
    endpoint: runtime_paths::RuntimeEndpoint,
    runtime: Arc<DaemonQueryRuntime>,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    /// A daemon whose every Workspace shares one scripted watcher; the
    /// global config is written before the daemon starts.
    async fn start(label: &str, global_config: Option<&str>) -> Self {
        let home = TestDir::create(&format!("{label}-home"));
        let global = GlobalPaths::from_home(home.path());
        if let Some(body) = global_config {
            fs::create_dir_all(&global.root).expect("home root");
            fs::write(&global.config_file, format!("format_version = 1\n{body}")).expect("config");
        }
        let script = Script::default();
        let factory: WatchFactory = {
            let script = script.clone();
            Arc::new(move |_root: &Path| {
                Ok(Box::new(ScriptedSource(script.clone())) as Box<dyn WatchSource>)
            })
        };
        let mut server = Server::bind_with_watch_factory(&global, factory)
            .await
            .expect("bind");
        let runtime = server.query_runtime();
        let endpoint = runtime_paths::resolve(&global);
        let server = tokio::spawn(async move {
            let _ = server.serve().await;
        });
        Self {
            _home: home,
            roots: Vec::new(),
            script,
            endpoint,
            runtime,
            server,
        }
    }

    /// A Workspace with `files`, optionally a Git repository, initialized
    /// through the daemon.
    async fn workspace(
        &mut self,
        label: &str,
        files: &[(&str, &str)],
        git_repo: bool,
        workspace_config: Option<&str>,
    ) -> (WorkspaceId, PathBuf) {
        let root = TestDir::create(label);
        for (rel, body) in files {
            write(root.path(), rel, body);
        }
        if let Some(body) = workspace_config {
            let paths = WorkspacePaths::from_root(root.path());
            fs::create_dir_all(&paths.root).expect("workspace config root");
            fs::write(&paths.config_file, format!("format_version = 1\n{body}"))
                .expect("workspace config");
        }
        if git_repo {
            git(root.path(), &["init", "-q", "."]);
            git(root.path(), &["config", "core.autocrlf", "false"]);
        }
        let path = root.path().to_path_buf();
        self.roots.push(root);
        let mut connection = self.connect().await;
        let response = send(
            &mut connection,
            Request::Init(InitRequest {
                path: path.to_string_lossy().into_owned(),
            }),
        )
        .await;
        let Response::Init(InitResponse { workspace_id, .. }) = response else {
            panic!("init failed: {response:?}")
        };
        (workspace_id.parse().expect("id"), path)
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
                client_kind: "i6-task6".to_owned(),
            }),
        )
        .await;
        connection
    }

    async fn baseline(&self, workspace: WorkspaceId) -> CommandBaseline {
        self.runtime
            .command_baseline(workspace)
            .await
            .expect("baseline")
    }

    async fn refresh(
        &self,
        workspace: WorkspaceId,
        baseline: &CommandBaseline,
    ) -> PostCommandRefreshReport {
        self.runtime
            .post_command_refresh(workspace, baseline.clone())
            .await
            .expect("post-command refresh")
    }

    async fn stats(&self, workspace: WorkspaceId) -> LifecycleStats {
        self.runtime
            .lifecycle_stats(workspace)
            .await
            .expect("workspace runtime")
    }

    /// Every file path a `find files` query sees, and whether it was
    /// current.
    async fn files(&self, workspace: WorkspaceId) -> (BTreeSet<String>, bool) {
        files_of(&self.runtime, workspace).await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn files_of(
    runtime: &DaemonQueryRuntime,
    workspace: WorkspaceId,
) -> (BTreeSet<String>, bool) {
    let (result, _) = runtime
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
            None,
        )
        .await
        .expect("find files");
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

async fn send(connection: &mut ClientConnection, request: Request) -> Response {
    protocol::framing::write_message(connection, &request)
        .await
        .expect("write");
    protocol::framing::read_message(connection)
        .await
        .expect("read")
}

// ----------------------------------------------------------- index reads

fn index(root: &Path) -> Connection {
    Connection::open_with_flags(
        WorkspacePaths::from_root(root).index_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("index.db")
}

/// The basis as stored: clock, stable generation, incarnation.
fn stored_basis(root: &Path) -> IndexBasis {
    let db = index(root);
    let (workspace_revision, generation_no, generation_basis_revision) = db
        .query_row(
            "SELECT c.current_workspace_revision, g.generation_no, g.basis_workspace_revision \
             FROM workspace_clock c JOIN generation g ON g.id = c.stable_generation_id",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("clock");
    let incarnation: Vec<u8> = db
        .query_row(
            "SELECT index_incarnation_uid FROM db_meta WHERE id = 0",
            [],
            |row| row.get(0),
        )
        .expect("incarnation");
    IndexBasis {
        index_incarnation: IndexIncarnationId::from_bytes(incarnation.try_into().expect("16")),
        workspace_revision,
        generation_no,
        generation_basis_revision,
    }
}

fn stored_active(root: &Path) -> HashMap<ResourceId, String> {
    let db = index(root);
    let mut statement = db
        .prepare("SELECT uid, resource_revision FROM resource WHERE state = 'ACTIVE'")
        .expect("prepare");
    statement
        .query_map([], |row| {
            let uid: Vec<u8> = row.get(0)?;
            Ok((
                ResourceId::from_bytes(uid.try_into().expect("16")),
                row.get(1)?,
            ))
        })
        .expect("query")
        .map(|row| row.expect("row"))
        .collect()
}

/// (path, revision, state) of one Resource row, whatever its state.
fn stored_row(root: &Path, id: ResourceId) -> (String, String, String) {
    index(root)
        .query_row(
            "SELECT path_rel, resource_revision, state FROM resource WHERE uid = ?1",
            [id.to_bytes().to_vec()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("resource row")
}

fn revision(basis: &IndexBasis) -> u64 {
    basis.workspace_revision.parse().expect("numeric revision")
}

fn summary(report: &PostCommandRefreshReport) -> Vec<(ResourceDeltaKind, &str)> {
    report
        .changes
        .iter()
        .map(|change| (change.kind, change.path.as_str()))
        .collect()
}

/// Replace a file's bytes with different ones of the same length and put
/// its mtime back: nothing cheap tells it changed.
fn rewrite_keeping_size_and_mtime(path: &Path) {
    let mtime = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .expect("mtime");
    let body = fs::read_to_string(path).expect("read");
    let changed = body.replace("42", "43");
    assert_eq!(changed.len(), body.len());
    assert_ne!(changed, body);
    fs::write(path, changed).expect("rewrite");
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open")
        .set_modified(mtime)
        .expect("restore mtime");
    assert_eq!(
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .expect("mtime"),
        mtime
    );
}

const FILES: &[(&str, &str)] = &[("src/shared.ts", SHARED_TS), ("src/app.ts", APP_TS)];

// ----------------------------------------------------------------- tests

/// The baseline runs the freshness barrier first: a change the watcher
/// reported but nothing settled yet is in it. Its basis and revisions are
/// exactly the index's; it fails when no current basis can be proven.
#[tokio::test(flavor = "multi_thread")]
async fn the_baseline_is_current_and_exact() {
    let mut fixture = Fixture::start("baseline", None).await;
    let (workspace, root) = fixture.workspace("baseline-ws", FILES, false, None).await;
    let initial = stored_basis(&root);

    write(&root, "src/app.ts", "export const changed = true;\n");
    fixture.script.push(RawWatchEvent::Modified {
        path: root.join("src/app.ts"),
    });
    let baseline = fixture.baseline(workspace).await;
    assert!(
        revision(&baseline.basis) > revision(&initial),
        "unsettled change not settled"
    );
    assert_eq!(baseline.basis, stored_basis(&root));
    assert_eq!(
        baseline.basis.generation_basis_revision,
        baseline.basis.workspace_revision
    );
    assert_eq!(baseline.resources, stored_active(&root));

    // The baseline did not start a command boundary of its own.
    let again = fixture.baseline(workspace).await;
    assert_eq!(again, baseline);

    // No current basis to prove: an unregistered Workspace, and (where
    // the OS lets a test remove a directory the daemon holds files in)
    // one that went missing.
    assert!(matches!(
        fixture
            .runtime
            .command_baseline(WorkspaceId::generate())
            .await,
        Err(CommandBasisError::Refresh(_))
    ));
    #[cfg(unix)]
    {
        fs::remove_dir_all(&root).expect("remove the workspace");
        assert!(matches!(
            fixture.runtime.command_baseline(workspace).await,
            Err(CommandBasisError::Refresh(_))
        ));
    }
}

/// Nothing delivered by the watcher, the index CURRENT: the refresh still
/// runs a verified reconcile and finds a create, a modify and a delete.
#[tokio::test(flavor = "multi_thread")]
async fn a_forced_reconcile_finds_what_the_watcher_never_reported() {
    let mut fixture = Fixture::start("missed", None).await;
    let (workspace, root) = fixture.workspace("missed-ws", FILES, false, None).await;
    let baseline = fixture.baseline(workspace).await;
    let shared = *baseline
        .resources
        .keys()
        .find(|id| stored_row(&root, **id).0 == "src/shared.ts")
        .expect("shared.ts");
    let reconciles = fixture.stats(workspace).await.reconciles;

    write(&root, "src/new.ts", "export const fresh = 1;\n");
    write(&root, "src/app.ts", "export const app = 2;\n");
    fs::remove_file(root.join("src/shared.ts")).expect("delete");
    let report = fixture.refresh(workspace, &baseline).await;

    assert_eq!(fixture.stats(workspace).await.reconciles, reconciles + 1);
    assert_eq!(
        summary(&report),
        [
            (ResourceDeltaKind::Created, "src/new.ts"),
            (ResourceDeltaKind::Updated, "src/app.ts"),
            (ResourceDeltaKind::Deleted, "src/shared.ts"),
        ]
    );
    assert_eq!(
        (
            report.created_count,
            report.updated_count,
            report.deleted_count,
            report.total_changed()
        ),
        (1, 1, 1, 3)
    );
    let deleted = &report.changes[2];
    assert_eq!(deleted.resource_id, shared);
    let (path, tombstone_revision, state) = stored_row(&root, shared);
    assert_eq!(
        (path.as_str(), state.as_str()),
        ("src/shared.ts", "DELETED")
    );
    assert_eq!(deleted.resource_revision, tombstone_revision);
    let active = stored_active(&root);
    for change in &report.changes[..2] {
        assert_eq!(active[&change.resource_id], change.resource_revision);
    }

    assert_eq!(report.before, baseline.basis);
    assert_eq!(report.after, stored_basis(&root));
    assert!(revision(&report.after) > revision(&report.before));
    assert!(report.after.generation_no > report.before.generation_no);
    assert_eq!(
        report.after.generation_basis_revision,
        report.after.workspace_revision
    );
    assert_eq!(
        fixture.files(workspace).await,
        (
            BTreeSet::from(["src/app.ts".to_owned(), "src/new.ts".to_owned()]),
            true
        )
    );
}

/// Same length, other bytes, mtime put back, no event: still Updated.
#[tokio::test(flavor = "multi_thread")]
async fn a_same_size_same_mtime_write_is_updated() {
    let mut fixture = Fixture::start("same-size", None).await;
    let (workspace, root) = fixture.workspace("same-size-ws", FILES, false, None).await;
    let baseline = fixture.baseline(workspace).await;

    rewrite_keeping_size_and_mtime(&root.join("src/shared.ts"));
    let report = fixture.refresh(workspace, &baseline).await;

    assert_eq!(
        summary(&report),
        [(ResourceDeltaKind::Updated, "src/shared.ts")]
    );
    assert_eq!(report.total_changed(), 1);
    assert!(revision(&report.after) > revision(&report.before));
    assert!(report.after.generation_no > report.before.generation_no);
    assert_eq!(report.after, stored_basis(&root));
    assert!(fixture.files(workspace).await.1, "current");
}

/// No change: the forced reconcile runs, publishes nothing, and moves
/// neither the revision nor the generation. Brainprint writes nothing the
/// Workspace's Git status could see.
#[tokio::test(flavor = "multi_thread")]
async fn a_no_op_refresh_moves_nothing() {
    let mut fixture = Fixture::start("no-op", None).await;
    let (workspace, root) = fixture.workspace("no-op-ws", FILES, true, None).await;
    let status = git(&root, &["status", "--porcelain", "--untracked-files=all"]);
    let baseline = fixture.baseline(workspace).await;
    let before = fixture.stats(workspace).await;

    let report = fixture.refresh(workspace, &baseline).await;

    let after = fixture.stats(workspace).await;
    assert_eq!(after.reconciles, before.reconciles + 1, "forced");
    assert_eq!(after.reconcile_publications, before.reconcile_publications);
    assert!(report.changes.is_empty());
    assert_eq!(report.total_changed(), 0);
    assert_eq!(report.after, report.before);
    assert_eq!(
        git(&root, &["status", "--porcelain", "--untracked-files=all"]),
        status
    );
}

/// A new mtime on unchanged bytes is metadata, not a change.
#[tokio::test(flavor = "multi_thread")]
async fn metadata_only_drift_is_not_a_change() {
    let mut fixture = Fixture::start("metadata", None).await;
    let (workspace, root) = fixture.workspace("metadata-ws", FILES, false, None).await;
    let baseline = fixture.baseline(workspace).await;

    let path = root.join("src/app.ts");
    let later = SystemTime::now() + Duration::from_secs(3600);
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open")
        .set_modified(later)
        .expect("touch");
    let report = fixture.refresh(workspace, &baseline).await;

    assert!(report.changes.is_empty(), "{:?}", report.changes);
    assert_eq!(
        report.after.workspace_revision,
        report.before.workspace_revision
    );
    assert_eq!(report.after.generation_no, report.before.generation_no);
}

/// The watcher saw the change and the index published it while the
/// command ran; the final reconcile finds nothing new, yet the delta from
/// the baseline still has it.
#[tokio::test(flavor = "multi_thread")]
async fn a_publication_during_the_command_is_still_in_the_delta() {
    let mut fixture = Fixture::start("prepublished", None).await;
    let (workspace, root) = fixture
        .workspace("prepublished-ws", FILES, false, None)
        .await;
    let baseline = fixture.baseline(workspace).await;

    write(&root, "src/app.ts", "export const watched = 3;\n");
    fixture.script.push(RawWatchEvent::Modified {
        path: root.join("src/app.ts"),
    });
    let before = fixture.stats(workspace).await;
    assert!(fixture.files(workspace).await.1);
    let published = fixture.stats(workspace).await;
    assert_eq!(
        published.refresh_publications,
        before.refresh_publications + 1,
        "published before the command ended"
    );
    let mid = stored_basis(&root);
    assert!(revision(&mid) > revision(&baseline.basis));

    let report = fixture.refresh(workspace, &baseline).await;

    let after = fixture.stats(workspace).await;
    assert_eq!(after.reconciles, published.reconciles + 1);
    assert_eq!(
        after.reconcile_publications, published.reconcile_publications,
        "the final reconcile itself was a no-op"
    );
    assert_eq!(report.after, mid);
    assert_eq!(
        summary(&report),
        [(ResourceDeltaKind::Updated, "src/app.ts")]
    );
}

/// Created and deleted again within the command: the net delta is empty
/// even though the index saw the file come and go.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_then_delete_is_net_nothing() {
    let mut fixture = Fixture::start("transient", None).await;
    let (workspace, root) = fixture.workspace("transient-ws", FILES, false, None).await;
    let baseline = fixture.baseline(workspace).await;

    write(&root, "src/tmp.ts", "export const tmp = 1;\n");
    fixture.script.push(RawWatchEvent::Created {
        path: root.join("src/tmp.ts"),
    });
    assert!(fixture.files(workspace).await.0.contains("src/tmp.ts"));
    fs::remove_file(root.join("src/tmp.ts")).expect("delete");
    let report = fixture.refresh(workspace, &baseline).await;

    assert!(report.changes.is_empty(), "{:?}", report.changes);
    assert!(revision(&report.after) > revision(&report.before));
    assert!(!fixture.files(workspace).await.0.contains("src/tmp.ts"));
}

#[tokio::test(flavor = "multi_thread")]
async fn bulk_changes_are_counted_exactly_and_ordered() {
    let mut fixture = Fixture::start("bulk", None).await;
    let files = [
        ("src/shared.ts", SHARED_TS),
        ("src/app.ts", APP_TS),
        ("src/extra.ts", EXTRA_TS),
        ("src/zeta.ts", EXTRA_TS),
    ];
    let (workspace, root) = fixture.workspace("bulk-ws", &files, false, None).await;
    let baseline = fixture.baseline(workspace).await;

    write(&root, "src/added.ts", "export const added = 1;\n");
    write(&root, "src/zeta.ts", "export const zeta = 2;\n");
    write(&root, "src/app.ts", "export const app = 2;\n");
    fs::remove_file(root.join("src/extra.ts")).expect("delete");
    let report = fixture.refresh(workspace, &baseline).await;

    assert_eq!(
        summary(&report),
        [
            (ResourceDeltaKind::Created, "src/added.ts"),
            (ResourceDeltaKind::Updated, "src/app.ts"),
            (ResourceDeltaKind::Updated, "src/zeta.ts"),
            (ResourceDeltaKind::Deleted, "src/extra.ts"),
        ]
    );
    assert_eq!(
        (
            report.created_count,
            report.updated_count,
            report.deleted_count,
            report.total_changed()
        ),
        (1, 2, 1, 4)
    );
    let active = stored_active(&root);
    for change in &report.changes {
        let (path, revision, _) = stored_row(&root, change.resource_id);
        assert_eq!(
            (path.as_str(), &revision),
            (change.path.as_str(), &change.resource_revision)
        );
        if change.kind == ResourceDeltaKind::Updated {
            assert_ne!(baseline.resources[&change.resource_id], revision);
            assert_eq!(active[&change.resource_id], revision);
        }
    }
}

/// A baseline from another physical index is not this index's history.
#[tokio::test(flavor = "multi_thread")]
async fn another_index_incarnation_is_refused() {
    let mut fixture = Fixture::start("incarnation", None).await;
    let (workspace, root) = fixture
        .workspace("incarnation-ws", FILES, false, None)
        .await;
    let mut baseline = fixture.baseline(workspace).await;
    baseline.basis.index_incarnation = IndexIncarnationId::generate();

    write(&root, "src/app.ts", "export const app = 9;\n");
    assert_eq!(
        fixture
            .runtime
            .post_command_refresh(workspace, baseline)
            .await,
        Err(CommandBasisError::IndexIncarnationChanged)
    );
}

/// One Workspace's refresh leaves another's basis, inventory and
/// lifecycle untouched.
#[tokio::test(flavor = "multi_thread")]
async fn another_workspace_is_untouched() {
    let mut fixture = Fixture::start("isolation", None).await;
    let (a, a_root) = fixture.workspace("isolation-a", FILES, false, None).await;
    let (b, b_root) = fixture.workspace("isolation-b", FILES, false, None).await;
    let b_before = fixture.baseline(b).await;
    let b_stats = fixture.stats(b).await;
    let b_index = (stored_basis(&b_root), stored_active(&b_root));

    let baseline = fixture.baseline(a).await;
    write(&a_root, "src/app.ts", "export const a = 1;\n");
    let report = fixture.refresh(a, &baseline).await;
    assert_eq!(report.total_changed(), 1);

    assert_eq!(fixture.stats(b).await, b_stats);
    assert_eq!((stored_basis(&b_root), stored_active(&b_root)), b_index);
    assert_eq!(fixture.baseline(b).await, b_before);
}

/// The refresh withdraws semantic contributions as any publication does
/// but never starts a backend: a registered Rust backend stays asleep.
#[tokio::test(flavor = "multi_thread")]
async fn no_semantic_backend_is_started() {
    // Any runnable file answering `--version` registers the family; the
    // test only proves nothing tries to start it.
    let executable = which("git").expect("git on PATH");
    let mut fixture = Fixture::start(
        "semantic",
        Some(&format!(
            "\n[semantic_backends.rust]\nexecutable = '{}'\n",
            executable.display()
        )),
    )
    .await;
    let files = [
        (
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("src/lib.rs", "pub fn helper() -> u32 {\n    42\n}\n"),
    ];
    let (workspace, root) = fixture
        .workspace(
            "semantic-ws",
            &files,
            false,
            Some("project_execution_trust = \"Trusted\"\n"),
        )
        .await;
    let stats = fixture
        .runtime
        .semantic_stats(workspace)
        .await
        .expect("semantic runtime");
    assert_eq!(stats.registered, vec!["rust"], "{stats:?}");
    assert_eq!(stats.backend_starts, 0);

    let baseline = fixture.baseline(workspace).await;
    write(&root, "src/lib.rs", "pub fn helper() -> u32 {\n    7\n}\n");
    let report = fixture.refresh(workspace, &baseline).await;
    assert_eq!(
        summary(&report),
        [(ResourceDeltaKind::Updated, "src/lib.rs")]
    );

    let stats = fixture
        .runtime
        .semantic_stats(workspace)
        .await
        .expect("semantic runtime");
    assert_eq!(stats.backend_starts, 0, "{stats:?}");
    assert_eq!(stats.live_runtimes, 0);
}

/// Queries racing the refresh on the same worker each see one whole basis
/// -- the old inventory or the new one, never a mix -- while another
/// Workspace's worker keeps answering.
#[tokio::test(flavor = "multi_thread")]
async fn the_worker_serializes_the_refresh() {
    let mut fixture = Fixture::start("serial", None).await;
    let (a, a_root) = fixture.workspace("serial-a", FILES, false, None).await;
    let (b, _) = fixture.workspace("serial-b", FILES, false, None).await;
    let baseline = fixture.baseline(a).await;
    let (old, _) = fixture.files(a).await;
    let added: Vec<String> = (0..120)
        .map(|index| format!("src/gen/g{index:03}.ts"))
        .collect();
    for path in &added {
        write(&a_root, path, "export const generated = 1;\n");
    }
    let new: BTreeSet<String> = old.iter().cloned().chain(added.iter().cloned()).collect();

    let runtime = &fixture.runtime;
    let queries = (0..8).map(|_| files_of(runtime, a));
    let (report, seen, other) = tokio::join!(
        runtime.post_command_refresh(a, baseline),
        one_after_another(queries),
        files_of(runtime, b),
    );
    let report = report.expect("refresh");
    // The 120 files and their new directory.
    assert_eq!(report.created_count, 121);
    for (files, current) in seen {
        assert!(current);
        assert!(
            files == old || files == new,
            "a mixed basis: {} files",
            files.len()
        );
    }
    assert_eq!(other.0.len(), 2);
    assert_eq!(fixture.files(a).await.0, new);
}

/// Each query sent once the previous one answered, all while the refresh
/// is in flight.
async fn one_after_another<F: std::future::Future>(
    futures: impl Iterator<Item = F>,
) -> Vec<F::Output> {
    let handles: Vec<_> = futures.collect();
    let mut outputs = Vec::with_capacity(handles.len());
    for future in handles {
        outputs.push(future.await);
    }
    outputs
}

fn which(program: &str) -> Option<PathBuf> {
    let names = if cfg!(windows) {
        vec![format!("{program}.exe")]
    } else {
        vec![program.to_owned()]
    };
    env::split_paths(&env::var_os("PATH")?)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|path| path.is_file())
}
