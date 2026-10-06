//! #55 step 2 acceptance: the pre-command baseline and the post-command
//! refresh wired into synchronous (#52) and managed (#54) verification,
//! and a managed Job attached as a Work Result's verification -- against
//! a real in-process daemon, through its own runtime API and (for the
//! synchronous run) the real protocol. Protocol 8: nothing here is wire.
//!
//! The watch source is gated and silent: it delivers only the events a
//! test pushes, so "the watcher saw nothing" is a fact, and a test can
//! hold the Workspace worker inside a refresh. Every post-command change
//! a query sees was therefore found by the forced verified reconcile.
//!
//! `harness = false`: this test binary is also the portable helper the
//! commands run (no shell). Helper ops, in order: `exit:<code>`,
//! `sleep:<ms>`, `mark:<path>` (append one byte), `write:<path>` (a
//! small TypeScript file), `swap:<path>` (`helper` -> `zelper`, same
//! size, mtime put back), `move:<from>|<to>` (rename).

use std::{
    collections::BTreeSet,
    env, fs,
    future::Future,
    io::Write as _,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use brainprint_core::{
    IndexIncarnationId, PROTOCOL_VERSION, VerificationJobId, WorkItemId, WorkspaceId,
    protocol::{
        self, ClientConnection, HandshakeRequest, InitRequest, InitResponse, Request, Response,
        query::*, work::*,
    },
};
use brainprint_daemon::{
    query::{
        AttachError, DaemonQueryRuntime, EndReason, JobFinishedPayload, ManagedError,
        ManagedEventPayload, PostCommandRefreshStored, RefreshFailure,
        lifecycle::{IndexBasis, LifecycleStats, ResourceDeltaKind, WatchFactory},
    },
    runtime_paths,
    server::Server,
};
use brainprint_engine::{
    knowledge::WorkspaceKnowledgeStore,
    paths::{GlobalPaths, WorkspacePaths},
    verification::VerificationCommand,
    verification_job::{
        IdempotencyKey, NewVerificationJob, ProgressEvent, TerminalState, VerificationJobState,
        VerificationJobStore, request_fingerprint,
    },
    watch::{RawWatchEvent, WatchSource},
};
use rusqlite::{Connection, OpenFlags};

const HELPER: &str = "--brainprint-verification-helper";
/// Longer than any cancel: a helper still running would write it late.
const LATE_MS: u64 = 3000;
const MISSING: &str = "brainprint-i6-task6-no-such-command";

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(HELPER) {
        helper(&args[1..]);
        return;
    }
    let filter = args.iter().find(|arg| !arg.starts_with('-')).cloned();
    let tests: &[(&str, fn())] = &[
        (
            "sync_records_at_the_post_command_basis",
            sync_records_at_the_post_command_basis,
        ),
        (
            "sync_sees_a_same_size_same_mtime_change",
            sync_sees_a_same_size_same_mtime_change,
        ),
        (
            "sync_without_a_spawn_records_at_the_baseline",
            sync_without_a_spawn_records_at_the_baseline,
        ),
        (
            "sync_refresh_failure_stores_nothing",
            sync_refresh_failure_stores_nothing,
        ),
        (
            "sync_disconnect_still_refreshes_and_frees_the_slot",
            sync_disconnect_still_refreshes_and_frees_the_slot,
        ),
        (
            "the_slot_is_held_through_the_refresh",
            the_slot_is_held_through_the_refresh,
        ),
        (
            "managed_finish_refreshes_before_job_finished",
            managed_finish_refreshes_before_job_finished,
        ),
        (
            "managed_without_a_spawn_is_not_needed",
            managed_without_a_spawn_is_not_needed,
        ),
        (
            "managed_cancel_refreshes_before_job_cancelled",
            managed_cancel_refreshes_before_job_cancelled,
        ),
        (
            "managed_shutdown_defers_the_refresh",
            managed_shutdown_defers_the_refresh,
        ),
        (
            "v1_jobs_still_poll_and_replay_but_never_attach",
            v1_jobs_still_poll_and_replay_but_never_attach,
        ),
        (
            "a_finished_job_attaches_without_running_again",
            a_finished_job_attaches_without_running_again,
        ),
        (
            "unproven_or_foreign_jobs_are_refused",
            unproven_or_foreign_jobs_are_refused,
        ),
        (
            "the_worker_rechecks_the_basis_at_the_write",
            the_worker_rechecks_the_basis_at_the_write,
        ),
        (
            "protocol_is_9_and_workspace_schema_7",
            protocol_is_9_and_workspace_schema_7,
        ),
    ];
    let handles: Vec<_> = tests
        .iter()
        .filter(|(name, _)| filter.as_deref().is_none_or(|filter| name.contains(filter)))
        .map(|&(name, test)| (name, thread::spawn(test)))
        .collect();
    let total = handles.len();
    let mut failed = 0;
    for (name, handle) in handles {
        let ok = handle.join().is_ok();
        println!("test {name} ... {}", if ok { "ok" } else { "FAILED" });
        failed += usize::from(!ok);
    }
    println!("{} passed; {failed} failed", total - failed);
    if failed > 0 {
        process::exit(101);
    }
}

// ---------------------------------------------------------------- helper

fn helper(ops: &[String]) {
    for op in ops {
        let (name, value) = op.split_once(':').expect("op:value");
        match name {
            "exit" => process::exit(value.parse().expect("code")),
            "sleep" => thread::sleep(Duration::from_millis(value.parse().expect("ms"))),
            "mark" => fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(value)
                .and_then(|mut file| file.write_all(b"x"))
                .expect("marker"),
            "write" => {
                let path = Path::new(value);
                fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
                fs::write(path, "export const generated = 1;\n").expect("write");
            }
            "swap" => swap_keeping_size_and_mtime(Path::new(value)),
            "move" => {
                let (from, to) = value.split_once('|').expect("from|to");
                fs::rename(from, to).expect("move");
            }
            other => panic!("unknown helper op {other}"),
        }
    }
}

/// Other bytes, same length, mtime put back: nothing cheap tells.
fn swap_keeping_size_and_mtime(path: &Path) {
    let mtime = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .expect("mtime");
    let body = fs::read_to_string(path).expect("read");
    let changed = body.replace("helper", "zelper");
    assert_eq!((changed.len(), changed != body), (body.len(), true));
    fs::write(path, changed).expect("rewrite");
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open")
        .set_modified(mtime)
        .expect("restore mtime");
}

fn command(label: &str, ops: &[&str]) -> VerificationCommandWire {
    let exe = env::current_exe().expect("exe");
    VerificationCommandWire {
        label: label.to_owned(),
        argv: [exe.to_string_lossy().as_ref(), HELPER]
            .into_iter()
            .chain(ops.iter().copied())
            .map(str::to_owned)
            .collect(),
        cwd: None,
        env: Vec::new(),
        timeout_secs: 30,
        capture: None,
    }
}

fn missing() -> VerificationCommandWire {
    VerificationCommandWire {
        argv: vec![MISSING.to_owned()],
        ..command("missing", &[])
    }
}

fn batch(commands: Vec<VerificationCommandWire>) -> VerificationWire {
    VerificationWire { commands }
}

fn op(name: &str, path: &Path) -> String {
    format!("{name}:{}", path.display())
}

/// Only the Unix-only refresh failure counts runs.
#[cfg(unix)]
fn count(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

// --------------------------------------------------------------- harness

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let path = env::temp_dir().join(format!(
            "bp-i6t6s2-{label}-{}-{}",
            process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
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

#[derive(Default)]
struct GateState {
    events: Vec<RawWatchEvent>,
    armed: bool,
    holding: bool,
}

/// The test's watcher: silent unless pushed to; while armed, a drain --
/// and so the Workspace worker -- waits inside it.
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<GateState>, Condvar)>);

impl Gate {
    fn push(&self, event: RawWatchEvent) {
        self.0.0.lock().expect("gate").events.push(event);
    }

    fn arm(&self) {
        self.0.0.lock().expect("gate").armed = true;
    }

    /// Until the worker is held in a drain.
    fn wait_held(&self) {
        let (state, changed) = &*self.0;
        let guard = state.lock().expect("gate");
        let (guard, timeout) = changed
            .wait_timeout_while(guard, Duration::from_secs(30), |state| !state.holding)
            .expect("gate");
        assert!(
            !timeout.timed_out() && guard.holding,
            "the worker never drained"
        );
    }

    fn open(&self) {
        self.0.0.lock().expect("gate").armed = false;
        self.0.1.notify_all();
    }
}

struct GatedSource(Gate);

impl WatchSource for GatedSource {
    fn drain(&mut self) -> Vec<RawWatchEvent> {
        let (state, changed) = &*self.0.0;
        let mut guard = state.lock().expect("gate");
        while guard.armed {
            guard.holding = true;
            changed.notify_all();
            guard = changed.wait(guard).expect("gate");
        }
        guard.holding = false;
        std::mem::take(&mut guard.events)
    }
}

fn factory(gate: &Gate) -> WatchFactory {
    let gate = gate.clone();
    Arc::new(move |_root: &Path| Ok(Box::new(GatedSource(gate.clone())) as Box<dyn WatchSource>))
}

/// One initialized Workspace of the fixture's daemon.
struct Ws {
    id: WorkspaceId,
    root: PathBuf,
    paths: WorkspacePaths,
}

/// An in-process daemon whose Workspaces share one gated watcher.
struct Fixture {
    home: TestDir,
    global: GlobalPaths,
    roots: Vec<TestDir>,
    gate: Gate,
    endpoint: runtime_paths::RuntimeEndpoint,
    runtime: Arc<DaemonQueryRuntime>,
    connection: ClientConnection,
    ws: Ws,
    _server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new(label: &str) -> Self {
        let home = TestDir::create(&format!("{label}-home"));
        let global = GlobalPaths::from_home(home.path());
        let gate = Gate::default();
        let mut server = Server::bind_with_watch_factory(&global, factory(&gate))
            .await
            .expect("bind");
        let runtime = server.query_runtime();
        let endpoint = runtime_paths::resolve(&global);
        let server = tokio::spawn(async move {
            let _ = server.serve().await;
        });
        let mut connection = connect(&endpoint).await;
        let mut roots = Vec::new();
        let ws = workspace(&mut connection, &mut roots, label).await;
        Self {
            home,
            global,
            roots,
            gate,
            endpoint,
            runtime,
            connection,
            ws,
            _server: server,
        }
    }

    async fn other_workspace(&mut self, label: &str) -> Ws {
        workspace(&mut self.connection, &mut self.roots, label).await
    }

    fn marker(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    fn mark(&self, name: &str) -> String {
        op("mark", &self.marker(name))
    }

    async fn stats(&self) -> LifecycleStats {
        self.runtime
            .lifecycle_stats(self.ws.id)
            .await
            .expect("runtime")
    }

    async fn files(&self) -> (BTreeSet<String>, bool) {
        files_of(&self.runtime, self.ws.id).await
    }

    async fn started(&self, ws: &Ws) -> WorkItemId {
        let operation = WorkOperationWire::Start(WorkStartWire {
            work_item: WorkStartItemWire::New(NewWorkItemWire {
                source_kind: WorkItemSourceKindWire::Issue,
                source_ref: Some("#55".to_owned()),
                title: None,
                goal: "verify".to_owned(),
            }),
            head: None,
            git: GitObservationWire::Unknown,
            owner_agent: None,
        });
        match self.runtime.work(ws.id, operation).await {
            WorkResponse::Started(started) => started.working_state.work_item,
            other => panic!("expected Started, got {other:?}"),
        }
    }

    /// A synchronous verification through the protocol.
    async fn verify(&mut self, input: WorkResultInputWire) -> WorkResponse {
        work(&mut self.connection, &self.ws.paths, input).await
    }

    async fn job(&self, key: &str, commands: Vec<VerificationCommandWire>) -> VerificationJobId {
        self.runtime
            .managed_start(self.ws.id, key, batch(commands))
            .await
            .expect("accepted")
            .job_id
    }

    /// The terminal state and the terminal event's payload.
    async fn terminal(
        &self,
        job: VerificationJobId,
    ) -> (VerificationJobState, ManagedEventPayload) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let poll = self
                .runtime
                .managed_poll(self.ws.id, job, 0, 64)
                .await
                .expect("poll");
            if poll.job.state != VerificationJobState::Running {
                let last = poll.events.last().expect("events").payload.clone();
                return (poll.job.state, last);
            }
            assert!(Instant::now() < deadline, "the Job never ended");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A FINISHED Job whose command wrote `rel`.
    async fn finished_job(&self, key: &str, rel: &str) -> VerificationJobId {
        let job = self
            .job(
                key,
                vec![command("gen", &[&op("write", &self.ws.root.join(rel))])],
            )
            .await;
        let (state, _) = self.terminal(job).await;
        assert_eq!(state, VerificationJobState::Finished);
        job
    }
}

async fn connect(endpoint: &runtime_paths::RuntimeEndpoint) -> ClientConnection {
    #[cfg(unix)]
    let connection = ClientConnection::connect(&endpoint.socket_path).await;
    #[cfg(windows)]
    let connection = ClientConnection::connect(&endpoint.pipe_name).await;
    let mut connection = connection.expect("connect");
    send(
        &mut connection,
        &Request::Handshake(HandshakeRequest {
            protocol_version: PROTOCOL_VERSION,
            client_kind: "i6-task6-step2".to_owned(),
        }),
    )
    .await;
    connection
}

async fn workspace(connection: &mut ClientConnection, roots: &mut Vec<TestDir>, label: &str) -> Ws {
    let root = TestDir::create(&format!("{label}-ws"));
    for (rel, body) in [("src/shared.ts", SHARED_TS), ("src/app.ts", APP_TS)] {
        let path = root.path().join(rel);
        fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        fs::write(path, body).expect("write");
    }
    let path = root.path().to_path_buf();
    roots.push(root);
    let response = send(
        connection,
        &Request::Init(InitRequest {
            path: path.to_string_lossy().into_owned(),
        }),
    )
    .await;
    let Response::Init(InitResponse { workspace_id, .. }) = response else {
        panic!("init failed: {response:?}")
    };
    Ws {
        id: workspace_id.parse().expect("id"),
        paths: WorkspacePaths::from_root(&path),
        root: path,
    }
}

async fn send(connection: &mut ClientConnection, request: &Request) -> Response {
    protocol::framing::write_message(connection, request)
        .await
        .expect("write");
    protocol::framing::read_message(connection)
        .await
        .expect("read")
}

fn work_request(paths: &WorkspacePaths, input: WorkResultInputWire) -> Request {
    Request::Work(WorkRequest {
        workspace: WorkspaceSelectorWire::Locator {
            path: paths.workspace_root.to_string_lossy().into_owned(),
        },
        operation: WorkOperationWire::Result(input),
    })
}

async fn work(
    connection: &mut ClientConnection,
    paths: &WorkspacePaths,
    input: WorkResultInputWire,
) -> WorkResponse {
    match send(connection, &work_request(paths, input)).await {
        Response::Work(response) => response,
        other => panic!("expected Work, got {other:?}"),
    }
}

fn input(work_item: WorkItemId, verification: Option<VerificationWire>) -> WorkResultInputWire {
    WorkResultInputWire {
        work_item,
        outcome: WorkOutcomeWire::Partial,
        summary: "done".to_owned(),
        commit_id: None,
        verification_summary: None,
        verification,
        verification_job: None,
        git: GitObservationWire::Unknown,
        change_set: None,
    }
}

fn recorded(response: WorkResponse) -> WorkRecordedWire {
    match response {
        WorkResponse::Recorded(recorded) => recorded,
        other => panic!("expected Recorded, got {other:?}"),
    }
}

fn failed(response: WorkResponse) -> WorkFailureWire {
    match response {
        WorkResponse::Failed(failure) => failure,
        other => panic!("expected Failed, got {other:?}"),
    }
}

fn outcomes(results: &[CommandResultWire]) -> Vec<VerificationOutcomeWire> {
    results.iter().map(|result| result.outcome).collect()
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

// ------------------------------------------------------------ db reads

fn read_only(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("db")
}

/// The index's current basis, as stored.
fn stored_basis(ws: &Ws) -> IndexBasis {
    let db = read_only(&ws.paths.index_db);
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

/// The stored result's basis, as an index basis.
fn result_basis(ws: &Ws, work_item: WorkItemId) -> IndexBasis {
    let result = WorkspaceKnowledgeStore::open(&ws.paths.workspace_db)
        .expect("store")
        .get_work_result(work_item)
        .expect("read")
        .expect("result");
    IndexBasis {
        index_incarnation: result.result_index_incarnation.expect("incarnation"),
        workspace_revision: result.result_workspace_revision.clone(),
        generation_no: result.result_generation_no.expect("generation"),
        generation_basis_revision: result.result_workspace_revision,
    }
}

fn result_job(ws: &Ws, work_item: WorkItemId) -> Option<VerificationJobId> {
    WorkspaceKnowledgeStore::open(&ws.paths.workspace_db)
        .expect("store")
        .get_work_result(work_item)
        .expect("read")
        .expect("result")
        .verification_job
}

/// `work_result` rows, and how many name a Job.
fn results(ws: &Ws) -> (i64, i64) {
    read_only(&ws.paths.workspace_db)
        .query_row(
            "SELECT COUNT(*), COUNT(verification_job_id) FROM work_result",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("count")
}

fn symbol_names(ws: &Ws, rel: &str) -> BTreeSet<String> {
    let db = read_only(&ws.paths.index_db);
    let mut statement = db
        .prepare(
            "SELECT s.name FROM symbol s JOIN resource r ON r.id = s.resource_id \
             WHERE r.state = 'ACTIVE' AND r.path_rel = ?1",
        )
        .expect("prepare");
    statement
        .query_map([rel], |row| row.get(0))
        .expect("query")
        .map(|row| row.expect("row"))
        .collect()
}

fn revision(basis: &IndexBasis) -> u64 {
    basis.workspace_revision.parse().expect("numeric")
}

fn refresh_of(payload: &ManagedEventPayload) -> &PostCommandRefreshStored {
    match payload {
        ManagedEventPayload::JobFinished(JobFinishedPayload {
            refresh: Some(refresh),
            ..
        }) => refresh,
        ManagedEventPayload::JobCancelled(ended)
        | ManagedEventPayload::JobInterrupted(ended)
        | ManagedEventPayload::JobInternalError(ended) => {
            ended.refresh.as_ref().expect("a v2 refresh")
        }
        other => panic!("not a v2 terminal payload: {other:?}"),
    }
}

/// `(kind, path)` of every stored change; the refresh must be Current.
fn current_changes(
    refresh: &PostCommandRefreshStored,
) -> (IndexBasis, Vec<(ResourceDeltaKind, String)>) {
    let PostCommandRefreshStored::Current {
        after,
        changes,
        total_changed,
        delivery_omitted,
        ..
    } = refresh
    else {
        panic!("expected a Current refresh, got {refresh:?}")
    };
    assert_eq!(
        (*total_changed, *delivery_omitted),
        (changes.len() as u64, 0)
    );
    (
        after.clone(),
        changes
            .iter()
            .map(|change| (change.kind, change.path.clone()))
            .collect(),
    )
}

// ------------------------------------------------------------ synchronous

/// The command creates and modifies files: the result is stored at the
/// basis the forced refresh proved, and the next query sees both changes
/// current with no reconcile of its own.
fn sync_records_at_the_post_command_basis() {
    block_on(async {
        let mut fixture = Fixture::new("sync-basis").await;
        let work_item = fixture.started(&fixture.ws).await;
        let before = stored_basis(&fixture.ws);
        let stats = fixture.stats().await;
        let root = fixture.ws.root.clone();
        let response = fixture
            .verify(input(
                work_item,
                Some(batch(vec![command(
                    "gen",
                    &[
                        &op("write", &root.join("src/generated.ts")),
                        &op("write", &root.join("src/app.ts")),
                    ],
                )])),
            ))
            .await;
        let recorded = recorded(response);
        assert_eq!(
            outcomes(&recorded.verification.expect("ran")),
            [VerificationOutcomeWire::Passed]
        );

        let after = stored_basis(&fixture.ws);
        assert!(revision(&after) > revision(&before));
        assert_eq!(result_basis(&fixture.ws, work_item), after);
        assert_eq!(result_job(&fixture.ws, work_item), None);
        assert_eq!(
            recorded.result.result_workspace_revision,
            after.workspace_revision
        );
        let refreshed = fixture.stats().await;
        assert_eq!(
            refreshed.reconciles,
            stats.reconciles + 1,
            "one forced refresh"
        );

        let (files, current) = fixture.files().await;
        assert!(current);
        assert!(files.contains("src/generated.ts"), "{files:?}");
        assert_eq!(fixture.stats().await.reconciles, refreshed.reconciles);
        let app = symbol_names(&fixture.ws, "src/app.ts");
        assert!(!app.contains("run"), "app.ts was rewritten: {app:?}");
    });
}

/// Same length, other bytes, mtime put back, no watcher event: the
/// refresh before the write still finds it.
fn sync_sees_a_same_size_same_mtime_change() {
    block_on(async {
        let mut fixture = Fixture::new("sync-same-size").await;
        let work_item = fixture.started(&fixture.ws).await;
        let before = stored_basis(&fixture.ws);
        assert!(symbol_names(&fixture.ws, "src/shared.ts").contains("helper"));
        let shared = fixture.ws.root.join("src/shared.ts");
        recorded(
            fixture
                .verify(input(
                    work_item,
                    Some(batch(vec![command("swap", &[&op("swap", &shared)])])),
                ))
                .await,
        );
        let after = stored_basis(&fixture.ws);
        assert!(revision(&after) > revision(&before));
        assert_eq!(result_basis(&fixture.ws, work_item), after);
        let names = symbol_names(&fixture.ws, "src/shared.ts");
        assert!(
            names.contains("zelper") && !names.contains("helper"),
            "{names:?}"
        );
        assert!(fixture.files().await.1);
    });
}

/// Nothing spawned (a missing executable, then a skipped command): no
/// forced reconcile, and the result is stored at the baseline.
fn sync_without_a_spawn_records_at_the_baseline() {
    block_on(async {
        let mut fixture = Fixture::new("sync-no-spawn").await;
        let work_item = fixture.started(&fixture.ws).await;
        let before = stored_basis(&fixture.ws);
        let stats = fixture.stats().await;
        let recorded = recorded(
            fixture
                .verify(input(
                    work_item,
                    Some(batch(vec![missing(), command("after", &["exit:0"])])),
                ))
                .await,
        );
        assert!(matches!(
            outcomes(&recorded.verification.expect("results"))[..],
            [
                VerificationOutcomeWire::NotStarted { .. },
                VerificationOutcomeWire::Skipped
            ]
        ));
        assert_eq!(fixture.stats().await.reconciles, stats.reconciles);
        assert_eq!(stored_basis(&fixture.ws), before);
        assert_eq!(result_basis(&fixture.ws, work_item), before);
    });
}

/// The refresh cannot prove a basis (the command took index.db away):
/// the commands' results come back, nothing is stored, the slot is free.
/// Unix only: Windows does not rename a file the daemon holds open.
fn sync_refresh_failure_stores_nothing() {
    #[cfg(unix)]
    block_on(async {
        let mut fixture = Fixture::new("sync-refresh-fail").await;
        let work_item = fixture.started(&fixture.ws).await;
        let index = fixture.ws.paths.index_db.clone();
        let away = index.with_extension("away");
        let move_op = format!("move:{}|{}", index.display(), away.display());
        let response = fixture
            .verify(input(
                work_item,
                Some(batch(vec![command(
                    "move",
                    &[&move_op, &fixture.mark("ran")],
                )])),
            ))
            .await;
        fs::rename(&away, &index).expect("restore index.db");

        let failure = failed(response);
        assert!(
            matches!(
                failure.error,
                WorkErrorWire::VerificationRefreshFailed {
                    reason: RefreshFailureWire::ReconcileFailed
                }
            ),
            "{failure:?}"
        );
        assert!(
            matches!(
                failure.refresh,
                Some(PostCommandRefreshWire::Failed {
                    before: Some(_),
                    reason: RefreshFailureWire::ReconcileFailed
                })
            ),
            "{failure:?}"
        );
        assert_eq!(
            outcomes(&failure.verification.expect("the command ran")),
            [VerificationOutcomeWire::Passed]
        );
        assert_eq!(count(&fixture.marker("ran")), 1);
        assert_eq!(results(&fixture.ws), (0, 0));

        // The slot was released: the next verification runs and records.
        recorded(
            fixture
                .verify(input(
                    work_item,
                    Some(batch(vec![command("next", &["exit:0"])])),
                ))
                .await,
        );
    });
}

/// The client leaves while the command runs: the tree ends, nothing is
/// stored or answered, the raw capture is removed -- and the daemon still
/// refreshes, so the file the command wrote is current without any
/// further reconcile, and then frees the slot.
fn sync_disconnect_still_refreshes_and_frees_the_slot() {
    block_on(async {
        let mut fixture = Fixture::new("sync-disconnect").await;
        let work_item = fixture.started(&fixture.ws).await;
        let stats = fixture.stats().await;
        let artifacts = fixture.runtime.artifacts().accounting();
        let mut slow = command(
            "slow",
            &[
                &op("write", &fixture.ws.root.join("src/left.ts")),
                &fixture.mark("started"),
                &format!("sleep:{LATE_MS}"),
                &fixture.mark("late"),
            ],
        );
        slow.capture = Some(VerificationCaptureWire {
            raw: true,
            diagnostics: None,
        });
        let mut other = connect(&fixture.endpoint).await;
        protocol::framing::write_message(
            &mut other,
            &work_request(&fixture.ws.paths, input(work_item, Some(batch(vec![slow])))),
        )
        .await
        .expect("write");
        wait_for(&fixture.marker("started"));
        drop(other);

        let deadline = Instant::now() + Duration::from_secs(30);
        while fixture.stats().await.reconciles == stats.reconciles {
            assert!(Instant::now() < deadline, "no refresh after the disconnect");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let refreshed = fixture.stats().await.reconciles;
        assert_eq!(refreshed, stats.reconciles + 1);
        assert_eq!(results(&fixture.ws), (0, 0));

        let (files, current) = fixture.files().await;
        assert!(current && files.contains("src/left.ts"), "{files:?}");
        assert_eq!(
            fixture.stats().await.reconciles,
            refreshed,
            "the query reconciled nothing"
        );

        // The slot comes free once the refresh is done.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let response = fixture
                .verify(input(
                    work_item,
                    Some(batch(vec![command("next", &["exit:0"])])),
                ))
                .await;
            match response {
                WorkResponse::Failed(failure)
                    if failure.error == WorkErrorWire::VerificationBusy =>
                {
                    assert!(Instant::now() < deadline, "the slot was never freed");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                other => {
                    recorded(other);
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(LATE_MS)).await;
        assert!(
            !fixture.marker("late").exists(),
            "the tree outlived the client"
        );
        assert_eq!(fixture.runtime.artifacts().accounting(), artifacts);
    });
}

/// The command has ended but its refresh is held on the worker: the
/// Workspace's slot is still taken, for a synchronous and a managed
/// verification alike, until the result is written.
fn the_slot_is_held_through_the_refresh() {
    block_on(async {
        let mut fixture = Fixture::new("slot-refresh").await;
        let work_item = fixture.started(&fixture.ws).await;
        let mut other = connect(&fixture.endpoint).await;
        let paths = fixture.ws.paths.clone();
        let first = input(
            work_item,
            Some(batch(vec![command(
                "first",
                &[&fixture.mark("started"), "sleep:300", &fixture.mark("done")],
            )])),
        );
        let running = tokio::spawn(async move { work(&mut other, &paths, first).await });
        wait_for(&fixture.marker("started"));
        fixture.gate.arm();
        wait_for(&fixture.marker("done"));
        fixture.gate.wait_held();
        tokio::time::sleep(Duration::from_millis(400)).await;

        let busy = failed(
            fixture
                .verify(input(
                    work_item,
                    Some(batch(vec![command("again", &[&fixture.mark("never")])])),
                ))
                .await,
        );
        assert_eq!(busy.error, WorkErrorWire::VerificationBusy);
        assert_eq!(
            fixture
                .runtime
                .managed_start(
                    fixture.ws.id,
                    "held",
                    batch(vec![command("never", &["exit:0"])])
                )
                .await,
            Err(ManagedError::VerificationBusy)
        );
        assert_eq!(results(&fixture.ws), (0, 0));

        fixture.gate.open();
        recorded(running.await.expect("first"));
        assert!(!fixture.marker("never").exists());
        let job = fixture
            .job("after", vec![command("after", &["exit:0"])])
            .await;
        assert_eq!(
            fixture.terminal(job).await.0,
            VerificationJobState::Finished
        );
    });
}

// ---------------------------------------------------------------- managed

/// The command writes a file: the refresh runs before JOB_FINISHED, whose
/// v2 payload holds the current basis and the change; the next query
/// sees it with no reconcile of its own.
fn managed_finish_refreshes_before_job_finished() {
    block_on(async {
        let fixture = Fixture::new("managed-finish").await;
        let before = stored_basis(&fixture.ws);
        let job = fixture.finished_job("finish", "src/generated.ts").await;
        let poll = fixture
            .runtime
            .managed_poll(fixture.ws.id, job, 0, 64)
            .await
            .expect("poll");
        let kinds: Vec<_> = poll
            .events
            .iter()
            .map(|event| std::mem::discriminant(&event.payload))
            .collect();
        assert_eq!(
            kinds.len(),
            4,
            "started, command started/finished, finished"
        );
        let ManagedEventPayload::JobFinished(finished) = &poll.events[3].payload else {
            panic!("JOB_FINISHED last")
        };
        assert_eq!(finished.v, 2);
        let (after, changes) = current_changes(finished.refresh.as_ref().expect("refresh"));
        assert_eq!(
            changes,
            [(ResourceDeltaKind::Created, "src/generated.ts".to_owned())]
        );
        assert_eq!(after, stored_basis(&fixture.ws));
        assert!(revision(&after) > revision(&before));
        let raw: String = read_only(&fixture.ws.paths.workspace_db)
            .query_row(
                "SELECT payload_json FROM verification_job_event WHERE kind = 'JOB_FINISHED'",
                [],
                |row| row.get(0),
            )
            .expect("terminal row");
        assert!(raw.starts_with(r#"{"v":2,"#), "{raw}");

        let reconciles = fixture.stats().await.reconciles;
        let (files, current) = fixture.files().await;
        assert!(current && files.contains("src/generated.ts"));
        assert_eq!(fixture.stats().await.reconciles, reconciles);
    });
}

/// Nothing spawned: NotNeeded at the baseline, no forced reconcile.
fn managed_without_a_spawn_is_not_needed() {
    block_on(async {
        let fixture = Fixture::new("managed-no-spawn").await;
        let before = stored_basis(&fixture.ws);
        let stats = fixture.stats().await;
        let job = fixture.job("nothing", vec![missing()]).await;
        let (state, payload) = fixture.terminal(job).await;
        assert_eq!(state, VerificationJobState::Finished);
        assert_eq!(
            refresh_of(&payload),
            &PostCommandRefreshStored::NotNeeded {
                basis: before.clone()
            }
        );
        assert_eq!(stored_basis(&fixture.ws), before);
        assert_eq!(fixture.stats().await.reconciles, stats.reconciles);
    });
}

/// Cancelled after the command wrote a file: the tree ends, the refresh
/// runs, then JOB_CANCELLED with the change.
fn managed_cancel_refreshes_before_job_cancelled() {
    block_on(async {
        let fixture = Fixture::new("managed-cancel").await;
        let job = fixture
            .job(
                "cancel",
                vec![command(
                    "slow",
                    &[
                        &op("write", &fixture.ws.root.join("src/cancelled.ts")),
                        &fixture.mark("started"),
                        &format!("sleep:{LATE_MS}"),
                        &fixture.mark("late"),
                    ],
                )],
            )
            .await;
        wait_for(&fixture.marker("started"));
        let state = fixture
            .runtime
            .managed_cancel(fixture.ws.id, job)
            .await
            .expect("cancel");
        assert_eq!(state, VerificationJobState::Cancelled);
        let (state, payload) = fixture.terminal(job).await;
        assert_eq!(state, VerificationJobState::Cancelled);
        let ManagedEventPayload::JobCancelled(ended) = &payload else {
            panic!("{payload:?}")
        };
        assert_eq!(ended.reason, EndReason::CallerCancelled);
        let (after, changes) = current_changes(refresh_of(&payload));
        assert_eq!(
            changes,
            [(ResourceDeltaKind::Created, "src/cancelled.ts".to_owned())]
        );
        assert_eq!(after, stored_basis(&fixture.ws));
        let (files, current) = fixture.files().await;
        assert!(current && files.contains("src/cancelled.ts"));
        tokio::time::sleep(Duration::from_millis(LATE_MS)).await;
        assert!(
            !fixture.marker("late").exists(),
            "the tree outlived the cancel"
        );
    });
}

/// A clean shutdown interrupts at once with the refresh deferred: no
/// verified reconcile runs; a later daemon's freshness barrier recovers.
fn managed_shutdown_defers_the_refresh() {
    block_on(async {
        let fixture = Fixture::new("managed-shutdown").await;
        let before = stored_basis(&fixture.ws);
        let job = fixture
            .job(
                "shutdown",
                vec![command(
                    "slow",
                    &[
                        &op("write", &fixture.ws.root.join("src/later.ts")),
                        &fixture.mark("started"),
                        &format!("sleep:{LATE_MS}"),
                    ],
                )],
            )
            .await;
        wait_for(&fixture.marker("started"));
        let reconciles = fixture.stats().await.reconciles;
        let began = Instant::now();
        fixture.runtime.managed_shutdown().await;
        assert!(began.elapsed() < Duration::from_millis(LATE_MS));
        assert_eq!(
            fixture.stats().await.reconciles,
            reconciles,
            "no verified refresh"
        );

        let (state, payload) = fixture.terminal(job).await;
        assert_eq!(state, VerificationJobState::Interrupted);
        let ManagedEventPayload::JobInterrupted(ended) = &payload else {
            panic!("{payload:?}")
        };
        assert_eq!(ended.reason, EndReason::DaemonShutdown);
        assert_eq!(
            ended.refresh,
            Some(PostCommandRefreshStored::DeferredDaemonShutdown {
                before: Some(before)
            })
        );

        // The next daemon's runtime activates with a verified reconcile.
        let next =
            DaemonQueryRuntime::with_watch_factory(&fixture.global, factory(&Gate::default()));
        let (files, current) = files_of(&next, fixture.ws.id).await;
        assert!(current && files.contains("src/later.ts"), "{files:?}");
    });
}

// ----------------------------------------------------------------- attach

fn engine_command(wire: &VerificationCommandWire) -> VerificationCommand {
    VerificationCommand {
        label: wire.label.clone(),
        argv: wire.argv.clone(),
        cwd: wire.cwd.clone(),
        env: wire.env.clone(),
        timeout_secs: wire.timeout_secs,
        capture: None,
    }
}

/// A Job written by a protocol-8 daemon (payload v1) still polls and
/// replays; it has no post-command basis, so it never attaches.
fn v1_jobs_still_poll_and_replay_but_never_attach() {
    block_on(async {
        let fixture = Fixture::new("v1").await;
        let legacy = command("legacy", &["exit:0"]);
        let job = VerificationJobId::generate();
        let mut store =
            VerificationJobStore::open_bound(fixture.ws.id, &fixture.ws.paths.workspace_db)
                .expect("jobs");
        store
            .create(&NewVerificationJob {
                uid: job,
                idempotency_key: &IdempotencyKey::parse("legacy").expect("key"),
                request_fingerprint: request_fingerprint(&[engine_command(&legacy)]),
                command_count: 1,
                started_payload_json: r#"{"v":1}"#,
            })
            .expect("v1 job");
        store
            .append(
                job,
                ProgressEvent::CommandStarted,
                r#"{"v":1,"index":0,"label":"legacy"}"#,
            )
            .expect("v1 event");
        store
            .finish(
                job,
                TerminalState::Finished,
                Some("legacy: passed"),
                r#"{"v":1,"verification_summary":"legacy: passed","results":[]}"#,
            )
            .expect("v1 finish");
        drop(store);

        let poll = fixture
            .runtime
            .managed_poll(fixture.ws.id, job, 0, 64)
            .await
            .expect("a v1 Job polls");
        assert_eq!(poll.events.len(), 3);
        let ManagedEventPayload::JobFinished(finished) = &poll.events[2].payload else {
            panic!("JOB_FINISHED")
        };
        assert_eq!((finished.v, &finished.refresh), (1, &None));
        let replayed = fixture
            .runtime
            .managed_start(fixture.ws.id, "legacy", batch(vec![legacy]))
            .await
            .expect("replay");
        assert!(replayed.replayed && replayed.job_id == job);

        let work_item = fixture.started(&fixture.ws).await;
        assert_eq!(
            fixture
                .runtime
                .managed_attach(fixture.ws.id, input(work_item, None), job)
                .await,
            Err(AttachError::NoRefreshBasis)
        );
        assert_eq!(results(&fixture.ws), (0, 0));
        assert_eq!(fixture.runtime.verification_runs(), 0);
    });
}

/// A FINISHED Job at the current basis becomes the result's verification:
/// its summary and its stable ID, nothing run again. One verification
/// source only. A later result without a Job clears the provenance.
fn a_finished_job_attaches_without_running_again() {
    block_on(async {
        let mut fixture = Fixture::new("attach").await;
        let job = fixture.finished_job("attach", "src/generated.ts").await;
        let summary = fixture
            .runtime
            .managed_poll(fixture.ws.id, job, 0, 1)
            .await
            .expect("poll")
            .job
            .final_summary
            .expect("summary");
        let runs = fixture.runtime.verification_runs();
        let work_item = fixture.started(&fixture.ws).await;

        let mut with_summary = input(work_item, None);
        with_summary.verification_summary = Some("mine".to_owned());
        let with_run = input(work_item, Some(batch(vec![command("x", &["exit:0"])])));
        for refused in [with_summary, with_run] {
            assert!(matches!(
                fixture
                    .runtime
                    .managed_attach(fixture.ws.id, refused, job)
                    .await,
                Err(AttachError::InvalidObservation(_))
            ));
        }
        assert_eq!(results(&fixture.ws), (0, 0));

        let attached = recorded(
            fixture
                .runtime
                .managed_attach(fixture.ws.id, input(work_item, None), job)
                .await
                .expect("attached"),
        );
        assert_eq!(
            attached.result.verification_summary.as_deref(),
            Some(summary.as_str())
        );
        assert_eq!(result_job(&fixture.ws, work_item), Some(job));
        assert_eq!(
            result_basis(&fixture.ws, work_item),
            stored_basis(&fixture.ws)
        );
        assert_eq!(
            fixture.runtime.verification_runs(),
            runs,
            "nothing ran again"
        );

        // Replaced by a caller's summary: the provenance goes with it.
        let mut by_hand = input(work_item, None);
        by_hand.verification_summary = Some("by hand".to_owned());
        recorded(
            fixture
                .runtime
                .work(fixture.ws.id, WorkOperationWire::Result(by_hand))
                .await,
        );
        assert_eq!(result_job(&fixture.ws, work_item), None);
        assert_eq!(results(&fixture.ws), (1, 0));

        // And by a synchronous verification.
        recorded(
            fixture
                .runtime
                .managed_attach(fixture.ws.id, input(work_item, None), job)
                .await
                .expect("attached again"),
        );
        assert_eq!(results(&fixture.ws), (1, 1));
        recorded(
            fixture
                .verify(input(
                    work_item,
                    Some(batch(vec![command("sync", &["exit:0"])])),
                ))
                .await,
        );
        assert_eq!(result_job(&fixture.ws, work_item), None);
        assert_eq!(results(&fixture.ws), (1, 0));
    });
}

/// Refused, with nothing written or run: a Job the Workspace has moved
/// past (Stale), one not FINISHED (RUNNING / CANCELLED / INTERRUPTED /
/// INTERNAL_ERROR), one whose refresh proved nothing, and another
/// Workspace's Job.
fn unproven_or_foreign_jobs_are_refused() {
    block_on(async {
        let mut fixture = Fixture::new("refused").await;
        let work_item = fixture.started(&fixture.ws).await;

        // Stale: the source changed after the Job, and a query published it.
        let stale = fixture.finished_job("stale", "src/generated.ts").await;
        fs::write(
            fixture.ws.root.join("src/app.ts"),
            "export const moved = 1;\n",
        )
        .expect("edit");
        fixture.gate.push(RawWatchEvent::Modified {
            path: fixture.ws.root.join("src/app.ts"),
        });
        assert!(fixture.files().await.1);
        let runs = fixture.runtime.verification_runs();
        assert_eq!(
            attach(&fixture, work_item, stale).await,
            Err(AttachError::Stale)
        );

        // RUNNING, then CANCELLED.
        let running = fixture
            .job(
                "running",
                vec![command(
                    "slow",
                    &[&fixture.mark("started"), &format!("sleep:{LATE_MS}")],
                )],
            )
            .await;
        wait_for(&fixture.marker("started"));
        assert_eq!(
            attach(&fixture, work_item, running).await,
            Err(AttachError::NotFinished(VerificationJobState::Running))
        );
        fixture
            .runtime
            .managed_cancel(fixture.ws.id, running)
            .await
            .expect("cancel");
        assert_eq!(
            attach(&fixture, work_item, running).await,
            Err(AttachError::NotFinished(VerificationJobState::Cancelled))
        );

        // INTERRUPTED and INTERNAL_ERROR rows; a FINISHED row whose refresh
        // failed.
        let mut store =
            VerificationJobStore::open_bound(fixture.ws.id, &fixture.ws.paths.workspace_db)
                .expect("jobs");
        let row = |store: &mut VerificationJobStore, key: &str| {
            let uid = VerificationJobId::generate();
            store
                .create(&NewVerificationJob {
                    uid,
                    idempotency_key: &IdempotencyKey::parse(key).expect("key"),
                    request_fingerprint: request_fingerprint(&[engine_command(&command(key, &[]))]),
                    command_count: 1,
                    started_payload_json: r#"{"v":2}"#,
                })
                .expect("job");
            uid
        };
        for (key, to, state) in [
            (
                "interrupted",
                TerminalState::Interrupted,
                VerificationJobState::Interrupted,
            ),
            (
                "internal",
                TerminalState::InternalError,
                VerificationJobState::InternalError,
            ),
        ] {
            let uid = row(&mut store, key);
            store
                .finish(uid, to, None, r#"{"v":2,"reason":"runner_failure"}"#)
                .expect("end");
            assert_eq!(
                attach(&fixture, work_item, uid).await,
                Err(AttachError::NotFinished(state))
            );
        }
        let unproven = row(&mut store, "unproven");
        let payload = serde_json::to_string(&JobFinishedPayload {
            v: 2,
            verification_summary: "unproven: passed".to_owned(),
            results: Vec::new(),
            refresh: Some(PostCommandRefreshStored::Failed {
                before: None,
                reason: RefreshFailure::ReconcileFailed,
            }),
        })
        .expect("encode");
        store
            .finish(
                unproven,
                TerminalState::Finished,
                Some("unproven: passed"),
                &payload,
            )
            .expect("finish");
        drop(store);
        assert_eq!(
            attach(&fixture, work_item, unproven).await,
            Err(AttachError::RefreshNotCurrent)
        );

        // Another Workspace's Job is not one of this Workspace's.
        let other = fixture.other_workspace("refused-other").await;
        let foreign = fixture
            .runtime
            .managed_start(other.id, "foreign", batch(vec![command("x", &["exit:0"])]))
            .await
            .expect("other Workspace")
            .job_id;
        let deadline = Instant::now() + Duration::from_secs(30);
        while fixture
            .runtime
            .managed_poll(other.id, foreign, 0, 1)
            .await
            .expect("poll")
            .job
            .state
            == VerificationJobState::Running
        {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            attach(&fixture, work_item, foreign).await,
            Err(AttachError::JobNotFound)
        );

        assert_eq!(results(&fixture.ws), (0, 0));
        assert_eq!(
            fixture.runtime.verification_runs(),
            runs + 2,
            "only the two new Jobs ran"
        );
    });
}

async fn attach(
    fixture: &Fixture,
    work_item: WorkItemId,
    job: VerificationJobId,
) -> Result<WorkResponse, AttachError> {
    fixture
        .runtime
        .managed_attach(fixture.ws.id, input(work_item, None), job)
        .await
}

/// The evidence is read at one basis; before the write, the source
/// changes and the watcher reports it. The daemon reads the Job nothing
/// further: the worker's own check right before the write -- after its
/// freshness barrier published the change -- refuses it.
fn the_worker_rechecks_the_basis_at_the_write() {
    block_on(async {
        let fixture = Fixture::new("toctou").await;
        let job = fixture.finished_job("toctou", "src/generated.ts").await;
        let work_item = fixture.started(&fixture.ws).await;
        let evidence = fixture
            .runtime
            .managed_evidence(fixture.ws.id, job)
            .await
            .expect("evidence");
        assert_eq!(evidence.basis, stored_basis(&fixture.ws));

        fs::write(
            fixture.ws.root.join("src/app.ts"),
            "export const raced = 1;\n",
        )
        .expect("edit");
        fixture.gate.push(RawWatchEvent::Modified {
            path: fixture.ws.root.join("src/app.ts"),
        });
        let basis = evidence.basis.clone();
        assert_eq!(
            fixture
                .runtime
                .record_managed_result(fixture.ws.id, input(work_item, None), evidence)
                .await,
            Err(AttachError::Stale)
        );
        assert!(revision(&stored_basis(&fixture.ws)) > revision(&basis));
        assert_eq!(results(&fixture.ws), (0, 0));
    });
}

fn protocol_is_9_and_workspace_schema_7() {
    assert_eq!(PROTOCOL_VERSION, 14);
    assert_eq!(
        brainprint_engine::schema::workspace::WORKSPACE_MIGRATIONS.len(),
        7
    );
    assert_eq!(
        brainprint_daemon::query::MAX_REFRESH_DELTA_WIRE_BYTES,
        256 * 1024
    );
    assert_eq!(
        brainprint_daemon::query::MANAGED_JOB_EVENT_PAYLOAD_VERSION,
        2
    );
}
