//! #54 step 2 acceptance: the daemon's managed verification Jobs through
//! its internal API (`managed_start` / `managed_poll` / `managed_cancel`,
//! `Server::shutdown`), against a real in-process daemon and Workspace.
//! The wire and CLI are step 3.
//!
//! `harness = false`: this test binary is also the portable helper the
//! commands run (no shell). Helper ops, in order: `exit:<code>`,
//! `sleep:<ms>`, `mark:<path>` (append one byte: the file length counts
//! runs), `out:<n>` (n bytes on stdout), `line:<text>` (one stdout line),
//! `spawn [ ops… ]` (a child left running).

use std::{
    env, fs,
    future::Future,
    io::Write as _,
    path::{Path, PathBuf},
    process::{self, Command},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use brainprint_core::{
    PROTOCOL_VERSION, VerificationJobId, WorkItemId, WorkspaceId,
    protocol::{
        self, ClientConnection, HandshakeRequest, HandshakeResponse, InitRequest, InitResponse,
        Request, Response,
        query::{WorkItemSourceKindWire, WorkspaceSelectorWire},
        work::*,
    },
};
use brainprint_daemon::{
    artifacts::{ArtifactPart, ArtifactStream},
    query::{
        CaptureProgress, CommandFinishedPayload, DaemonQueryRuntime, EndReason, ManagedError,
        ManagedEventPayload, ManagedPoll, ManagedStart,
    },
    runtime_paths,
    server::Server,
};
use brainprint_engine::{
    git_status,
    paths::{GlobalPaths, WorkspacePaths},
    verification_job::{
        IdempotencyKey, NewVerificationJob, RequestFingerprint, VerificationJobState,
        VerificationJobStore,
    },
};
use tokio::sync::oneshot;

const HELPER: &str = "--brainprint-verification-helper";
/// Longer than any cancel: a helper still running would write it late.
const LATE_MS: u64 = 3000;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(HELPER) {
        helper(&args[1..]);
        return;
    }
    let filter = args.iter().find(|arg| !arg.starts_with('-')).cloned();
    let tests: &[(&str, fn())] = &[
        (
            "a_key_replays_its_job_and_a_changed_request_conflicts",
            a_key_replays_its_job_and_a_changed_request_conflicts,
        ),
        (
            "slots_are_shared_with_synchronous_verification",
            slots_are_shared_with_synchronous_verification,
        ),
        (
            "events_follow_each_command_and_skip",
            events_follow_each_command_and_skip,
        ),
        (
            "a_missing_program_is_started_then_not_started",
            a_missing_program_is_started_then_not_started,
        ),
        (
            "progress_is_durable_while_running_and_polls_repeat",
            progress_is_durable_while_running_and_polls_repeat,
        ),
        ("cancel_ends_the_tree_once", cancel_ends_the_tree_once),
        (
            "clean_shutdown_interrupts_and_restart_reruns_nothing",
            clean_shutdown_interrupts_and_restart_reruns_nothing,
        ),
        (
            "stale_running_is_interrupted_once_on_first_use",
            stale_running_is_interrupted_once_on_first_use,
        ),
        (
            "raw_availability_follows_the_store_not_the_db",
            raw_availability_follows_the_store_not_the_db,
        ),
        (
            "diagnostic_items_are_stored_once_in_job_finished",
            diagnostic_items_are_stored_once_in_job_finished,
        ),
        (
            "a_failed_event_write_stops_the_batch",
            a_failed_event_write_stops_the_batch,
        ),
        (
            "request_values_are_never_stored",
            request_values_are_never_stored,
        ),
        (
            "the_worker_stays_free_and_git_stays_clean",
            the_worker_stays_free_and_git_stays_clean,
        ),
        (
            "invalid_requests_create_nothing",
            invalid_requests_create_nothing,
        ),
        (
            "protocol_is_8_and_workspace_schema_6",
            protocol_is_8_and_workspace_schema_6,
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

#[allow(
    clippy::zombie_processes,
    reason = "descendants are left running on purpose; the runner must end them"
)]
fn helper(ops: &[String]) {
    let mut index = 0;
    while index < ops.len() {
        let op = ops[index].as_str();
        index += 1;
        if op == "spawn" {
            let end = closing(ops, index);
            Command::new(env::current_exe().expect("exe"))
                .arg(HELPER)
                .args(&ops[index + 1..end])
                .spawn()
                .expect("spawn helper");
            index = end + 1;
            continue;
        }
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
            "out" => {
                let bytes = vec![b'o'; value.parse().expect("bytes")];
                std::io::stdout().write_all(&bytes).expect("stdout");
            }
            "line" => println!("{value}"),
            other => panic!("unknown helper op {other}"),
        }
    }
}

/// Index of the `]` matching the `[` at `open`.
fn closing(ops: &[String], open: usize) -> usize {
    assert_eq!(ops[open], "[");
    let mut depth = 0;
    for (index, op) in ops.iter().enumerate().skip(open) {
        match op.as_str() {
            "[" => depth += 1,
            "]" => {
                depth -= 1;
                if depth == 0 {
                    return index;
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced [ ]");
}

fn helper_argv(ops: &[&str]) -> Vec<String> {
    let exe = env::current_exe().expect("exe");
    [exe.to_string_lossy().as_ref(), HELPER]
        .into_iter()
        .chain(ops.iter().copied())
        .map(str::to_owned)
        .collect()
}

fn command(label: &str, ops: &[&str]) -> VerificationCommandWire {
    VerificationCommandWire {
        label: label.to_owned(),
        argv: helper_argv(ops),
        cwd: None,
        env: Vec::new(),
        timeout_secs: 30,
        capture: None,
    }
}

fn capturing(
    label: &str,
    ops: &[&str],
    raw: bool,
    diagnostics: Option<DiagnosticFormatWire>,
) -> VerificationCommandWire {
    VerificationCommandWire {
        capture: Some(VerificationCaptureWire { raw, diagnostics }),
        ..command(label, ops)
    }
}

fn batch(commands: Vec<VerificationCommandWire>) -> VerificationWire {
    VerificationWire { commands }
}

/// A long command that leaves a grandchild: both would write `late`.
fn slow_tree(started: &str, late: &str) -> VerificationWire {
    batch(vec![
        capturing(
            "tree",
            &[
                "spawn",
                "[",
                started,
                &format!("sleep:{LATE_MS}"),
                late,
                "]",
                &format!("sleep:{LATE_MS}"),
                late,
            ],
            true,
            None,
        ),
        command("after", &[late]),
    ])
}

/// How many times a helper wrote `path`.
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

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "brainprint-i6-task5-{label}-{}-{sequence}",
            process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("test dir should be created");
        Self(path)
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

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "brainprint-test")
        .env("GIT_AUTHOR_EMAIL", "test@brainprint.invalid")
        .env("GIT_COMMITTER_NAME", "brainprint-test")
        .env("GIT_COMMITTER_EMAIL", "test@brainprint.invalid")
        .output()
        .expect("git must be installed for these tests");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn workspace_tree(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("src");
    fs::write(root.join("src/app.rs"), "pub fn app() {}\n").expect("app.rs");
    git(root, &["init", "-q"]);
    git(root, &["config", "core.autocrlf", "false"]);
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "init"]);
}

async fn send(connection: &mut ClientConnection, request: &Request) -> Response {
    protocol::framing::write_message(connection, request)
        .await
        .expect("write should succeed");
    protocol::framing::read_message(connection)
        .await
        .expect("read should succeed")
}

async fn open_connection(endpoint: &runtime_paths::RuntimeEndpoint) -> ClientConnection {
    #[cfg(unix)]
    let connection = ClientConnection::connect(&endpoint.socket_path).await;
    #[cfg(windows)]
    let connection = ClientConnection::connect(&endpoint.pipe_name).await;
    let mut connection = connection.expect("connect should succeed");
    let response = send(
        &mut connection,
        &Request::Handshake(HandshakeRequest {
            protocol_version: PROTOCOL_VERSION,
            client_kind: "i6-task5-test".to_owned(),
        }),
    )
    .await;
    assert!(matches!(
        response,
        Response::Handshake(HandshakeResponse::Ok { .. })
    ));
    connection
}

async fn init(connection: &mut ClientConnection, root: &Path) -> WorkspacePaths {
    let response = send(
        connection,
        &Request::Init(InitRequest {
            path: root.to_string_lossy().into_owned(),
        }),
    )
    .await;
    let Response::Init(InitResponse { workspace_root, .. }) = response else {
        panic!("expected Init, got {response:?}")
    };
    WorkspacePaths::from_root(PathBuf::from(workspace_root))
}

async fn work(
    connection: &mut ClientConnection,
    paths: &WorkspacePaths,
    operation: WorkOperationWire,
) -> WorkResponse {
    let request = Request::Work(WorkRequest {
        workspace: WorkspaceSelectorWire::Locator {
            path: paths.workspace_root.to_string_lossy().into_owned(),
        },
        operation,
    });
    match send(connection, &request).await {
        Response::Work(response) => response,
        other => panic!("expected Work, got {other:?}"),
    }
}

async fn start_work_item(connection: &mut ClientConnection, paths: &WorkspacePaths) -> WorkItemId {
    let operation = WorkOperationWire::Start(WorkStartWire {
        work_item: WorkStartItemWire::New(NewWorkItemWire {
            source_kind: WorkItemSourceKindWire::Issue,
            source_ref: Some("#54".to_owned()),
            title: None,
            goal: "verify".to_owned(),
        }),
        head: None,
        git: GitObservationWire::Unknown,
        owner_agent: None,
    });
    match work(connection, paths, operation).await {
        WorkResponse::Started(started) => started.working_state.work_item,
        other => panic!("expected Started, got {other:?}"),
    }
}

fn sync_result(work_item: WorkItemId, verification: VerificationWire) -> WorkOperationWire {
    WorkOperationWire::Result(WorkResultInputWire {
        work_item,
        outcome: WorkOutcomeWire::Partial,
        summary: "done".to_owned(),
        commit_id: None,
        verification_summary: None,
        verification: Some(verification),
        git: GitObservationWire::Unknown,
        change_set: None,
    })
}

/// An in-process daemon with one initialized Git Workspace. The server
/// is handed back by [`Self::stop`] for [`Server::shutdown`].
struct Fixture {
    home: TestDir,
    root: TestDir,
    global_paths: GlobalPaths,
    endpoint: runtime_paths::RuntimeEndpoint,
    connection: ClientConnection,
    paths: WorkspacePaths,
    workspace: WorkspaceId,
    runtime: Arc<DaemonQueryRuntime>,
    stop: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<Server>,
}

type Served = (
    oneshot::Sender<()>,
    tokio::task::JoinHandle<Server>,
    Arc<DaemonQueryRuntime>,
);

async fn serve(global_paths: &GlobalPaths) -> Served {
    let mut server = Server::bind(global_paths).await.expect("bind");
    let runtime = server.query_runtime();
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        tokio::select! {
            _ = server.serve() => {}
            _ = stopped => {}
        }
        server
    });
    (stop, task, runtime)
}

impl Fixture {
    async fn new(label: &str) -> Self {
        let home = TestDir::create(&format!("{label}-home"));
        let root = TestDir::create(&format!("{label}-root"));
        workspace_tree(root.path());
        let global_paths = GlobalPaths::from_home(home.path());
        let (stop, server, runtime) = serve(&global_paths).await;
        let endpoint = runtime_paths::resolve(&global_paths);
        let mut connection = open_connection(&endpoint).await;
        let paths = init(&mut connection, root.path()).await;
        let workspace = runtime
            .resolve_workspace(paths.workspace_root.clone())
            .await
            .expect("registered");
        Self {
            home,
            root,
            global_paths,
            endpoint,
            connection,
            paths,
            workspace,
            runtime,
            stop,
            server,
        }
    }

    /// A marker path outside the Workspace (its writes never touch Git).
    fn marker(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    fn mark(&self, name: &str) -> String {
        format!("mark:{}", self.marker(name).display())
    }

    async fn start(
        &self,
        key: &str,
        verification: VerificationWire,
    ) -> Result<ManagedStart, ManagedError> {
        self.runtime
            .managed_start(self.workspace, key, verification)
            .await
    }

    async fn accepted(&self, key: &str, verification: VerificationWire) -> VerificationJobId {
        let start = self.start(key, verification).await.expect("accepted");
        assert!(!start.replayed);
        assert_eq!(start.state, VerificationJobState::Running);
        assert_eq!(start.last_seq, 1);
        start.job_id
    }

    async fn poll(&self, job: VerificationJobId, after: u64) -> ManagedPoll {
        self.runtime
            .managed_poll(self.workspace, job, after, 64)
            .await
            .expect("poll")
    }

    /// Every event, once the Job is terminal.
    async fn terminal(&self, job: VerificationJobId) -> ManagedPoll {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let poll = self.poll(job, 0).await;
            if poll.job.state != VerificationJobState::Running {
                assert!(!poll.has_more);
                return poll;
            }
            assert!(Instant::now() < deadline, "the Job never ended");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open_with_flags(
            &self.paths.workspace_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("workspace.db")
    }

    fn job_rows(&self, key: &str) -> i64 {
        self.db()
            .query_row(
                "SELECT COUNT(*) FROM verification_job WHERE idempotency_key = ?1",
                [key],
                |row| row.get(0),
            )
            .expect("count")
    }

    /// The stored events' kinds, in seq order.
    fn stored_kinds(&self, job: VerificationJobId) -> Vec<String> {
        let db = self.db();
        let mut statement = db
            .prepare(
                "SELECT e.kind FROM verification_job_event e JOIN verification_job j \
                 ON j.id = e.job_id WHERE j.uid = ?1 ORDER BY e.seq",
            )
            .expect("prepare");
        statement
            .query_map([job.to_bytes().to_vec()], |row| row.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("kinds")
    }
}

/// `(kind, index, outcome)` per event: the lifecycle at a glance.
fn shape(poll: &ManagedPoll) -> Vec<String> {
    poll.events
        .iter()
        .map(|event| match &event.payload {
            ManagedEventPayload::JobStarted(_) => "JobStarted".to_owned(),
            ManagedEventPayload::CommandStarted(started) => {
                format!("CommandStarted {} {}", started.index, started.label)
            }
            ManagedEventPayload::CommandFinished(finished) => format!(
                "CommandFinished {} {} {:?}",
                finished.index, finished.label, finished.outcome
            ),
            ManagedEventPayload::JobFinished(finished) => {
                format!("JobFinished {}", finished.results.len())
            }
            ManagedEventPayload::JobCancelled(ended) => format!("JobCancelled {:?}", ended.reason),
            ManagedEventPayload::JobInterrupted(ended) => {
                format!("JobInterrupted {:?}", ended.reason)
            }
            ManagedEventPayload::JobInternalError(ended) => {
                format!("JobInternalError {:?}", ended.reason)
            }
        })
        .collect()
}

fn command_finished(poll: &ManagedPoll, index: u32) -> &CommandFinishedPayload {
    poll.events
        .iter()
        .find_map(|event| match &event.payload {
            ManagedEventPayload::CommandFinished(finished) if finished.index == index => {
                Some(finished)
            }
            _ => None,
        })
        .expect("CommandFinished")
}

fn job_finished(poll: &ManagedPoll) -> &[CommandResultWire] {
    match &poll.events.last().expect("events").payload {
        ManagedEventPayload::JobFinished(finished) => &finished.results,
        other => panic!("expected JobFinished, got {other:?}"),
    }
}

fn terminal_events(fixture: &Fixture, job: VerificationJobId) -> usize {
    fixture
        .stored_kinds(job)
        .iter()
        .filter(|kind| {
            matches!(
                kind.as_str(),
                "JOB_FINISHED" | "JOB_CANCELLED" | "JOB_INTERRUPTED" | "JOB_INTERNAL_ERROR"
            )
        })
        .count()
}

// ----------------------------------------------------------------- tests

/// Same key + same request: the same Job, running or terminal, and the
/// marker command ran once. Same key + another request: a conflict that
/// runs nothing. A start returns while its command still runs, and the
/// dropped start result does not stop it.
fn a_key_replays_its_job_and_a_changed_request_conflicts() {
    block_on(async {
        let fixture = Fixture::new("replay").await;
        let request = || batch(vec![command("once", &["sleep:1500", &fixture.mark("ran")])]);
        let began = Instant::now();
        let first = fixture.accepted("agent:test:0001", request()).await;
        assert!(
            began.elapsed() < Duration::from_millis(1500),
            "start waited for the run"
        );
        let runs = fixture.runtime.verification_runs();

        let replay = fixture
            .start("agent:test:0001", request())
            .await
            .expect("replay");
        assert_eq!(replay.job_id, first);
        assert!(replay.replayed);
        assert_eq!(replay.state, VerificationJobState::Running);

        let done = fixture.terminal(first).await;
        assert_eq!(done.job.state, VerificationJobState::Finished);
        assert_eq!(count(&fixture.marker("ran")), 1);

        let replay = fixture
            .start("agent:test:0001", request())
            .await
            .expect("replay");
        assert_eq!(
            (
                replay.job_id,
                replay.state,
                replay.replayed,
                replay.last_seq
            ),
            (first, VerificationJobState::Finished, true, done.next_seq)
        );

        let mut changed = request();
        changed.commands[0].argv.push("note:changed".to_owned());
        assert_eq!(
            fixture.start("agent:test:0001", changed).await,
            Err(ManagedError::IdempotencyConflict)
        );
        thread::sleep(Duration::from_millis(200));
        assert_eq!(count(&fixture.marker("ran")), 1, "a replay ran the command");
        assert_eq!(
            fixture.runtime.verification_runs(),
            runs,
            "a replay took a slot"
        );
        assert_eq!(fixture.job_rows("agent:test:0001"), 1);
    });
}

/// One shared #52 Slots: managed and synchronous verification exclude
/// each other per Workspace, nothing is queued, and a busy start leaves no
/// row (the key stays free). Another Workspace runs meanwhile.
fn slots_are_shared_with_synchronous_verification() {
    block_on(async {
        let mut fixture = Fixture::new("busy").await;
        let work_item = start_work_item(&mut fixture.connection, &fixture.paths).await;
        let long = fixture
            .accepted(
                "long",
                batch(vec![command(
                    "long",
                    &[&fixture.mark("long"), "sleep:2500"],
                )]),
            )
            .await;
        wait_for(&fixture.marker("long"));

        let never = fixture.mark("never");
        assert_eq!(
            fixture
                .start("other", batch(vec![command("b", &[&never])]))
                .await,
            Err(ManagedError::VerificationBusy)
        );
        assert_eq!(fixture.job_rows("other"), 0, "a busy start created a Job");
        let busy = work(
            &mut fixture.connection,
            &fixture.paths,
            sync_result(work_item, batch(vec![command("sync", &[&never])])),
        )
        .await;
        let WorkResponse::Failed(failure) = busy else {
            panic!("expected Failed, got {busy:?}")
        };
        assert_eq!(failure.error, WorkErrorWire::VerificationBusy);

        // Another Workspace of the same daemon runs meanwhile.
        let other_root = TestDir::create("busy-other-root");
        workspace_tree(other_root.path());
        let mut other = open_connection(&fixture.endpoint).await;
        let other_paths = init(&mut other, other_root.path()).await;
        let elsewhere = fixture
            .runtime
            .resolve_workspace(other_paths.workspace_root.clone())
            .await
            .expect("registered");
        let parallel = fixture
            .runtime
            .managed_start(elsewhere, "long", batch(vec![command("fast", &["exit:0"])]))
            .await
            .expect("another Workspace is not busy");
        assert!(!parallel.replayed, "keys are per Workspace");

        assert_eq!(
            fixture.terminal(long).await.job.state,
            VerificationJobState::Finished
        );
        let retried = fixture
            .accepted("other", batch(vec![command("b", &["exit:0"])]))
            .await;
        fixture.terminal(retried).await;

        // Synchronous first: the managed start is busy.
        let started = fixture.mark("sync-started");
        let slow = batch(vec![command("slow", &[&started, "sleep:2000"])]);
        let mut sync = open_connection(&fixture.endpoint).await;
        let paths = fixture.paths.clone();
        let running =
            tokio::spawn(
                async move { work(&mut sync, &paths, sync_result(work_item, slow)).await },
            );
        wait_for(&fixture.marker("sync-started"));
        assert_eq!(
            fixture
                .start("during-sync", batch(vec![command("c", &[&never])]))
                .await,
            Err(ManagedError::VerificationBusy)
        );
        assert_eq!(fixture.job_rows("during-sync"), 0);
        assert!(matches!(
            running.await.expect("sync"),
            WorkResponse::Recorded(_)
        ));
        assert!(!fixture.marker("never").exists());
    });
}

/// pass, fail(3), skipped: exactly these events; no CommandStarted for
/// the skipped one; the Job is FINISHED although a command failed. A
/// cancel after the end changes nothing.
fn events_follow_each_command_and_skip() {
    block_on(async {
        let fixture = Fixture::new("events").await;
        let job = fixture
            .accepted(
                "events",
                batch(vec![
                    command("pass", &["exit:0"]),
                    command("fail", &["exit:3"]),
                    command("skip", &[&fixture.mark("skipped")]),
                ]),
            )
            .await;
        let done = fixture.terminal(job).await;
        assert_eq!(
            shape(&done),
            [
                "JobStarted",
                "CommandStarted 0 pass",
                "CommandFinished 0 pass Passed",
                "CommandStarted 1 fail",
                "CommandFinished 1 fail Failed { exit_code: 3 }",
                "CommandFinished 2 skip Skipped",
                "JobFinished 3",
            ]
        );
        let seqs: Vec<u64> = done.events.iter().map(|event| event.seq).collect();
        assert_eq!(seqs, (1..=7).collect::<Vec<_>>());
        assert_eq!(done.job.state, VerificationJobState::Finished);
        assert_eq!(
            done.job
                .final_summary
                .as_deref()
                .map(|summary| summary.contains("fail: failed exit=3")),
            Some(true)
        );
        assert!(!fixture.marker("skipped").exists());

        // Natural completion came first: a late cancel keeps FINISHED.
        assert_eq!(
            fixture.runtime.managed_cancel(fixture.workspace, job).await,
            Ok(VerificationJobState::Finished)
        );
        assert_eq!(fixture.stored_kinds(job).len(), 7);
    });
}

fn a_missing_program_is_started_then_not_started() {
    block_on(async {
        let fixture = Fixture::new("missing").await;
        let mut missing = command("missing", &[]);
        missing.argv = vec![fixture.marker("no-such-program").display().to_string()];
        let job = fixture
            .accepted(
                "missing",
                batch(vec![missing, command("later", &[&fixture.mark("later")])]),
            )
            .await;
        let done = fixture.terminal(job).await;
        assert_eq!(
            shape(&done),
            [
                "JobStarted",
                "CommandStarted 0 missing",
                "CommandFinished 0 missing NotStarted { reason: NotFound }",
                "CommandFinished 1 later Skipped",
                "JobFinished 2",
            ]
        );
        assert_eq!(done.job.state, VerificationJobState::Finished);
        assert!(!fixture.marker("later").exists());
    });
}

/// While the second command runs, the first's events and the second's
/// CommandStarted are already in workspace.db. The same cursor returns
/// the same events; polling reruns and interrupts nothing.
fn progress_is_durable_while_running_and_polls_repeat() {
    block_on(async {
        let fixture = Fixture::new("progress").await;
        let job = fixture
            .accepted(
                "progress",
                batch(vec![
                    command("first", &[&fixture.mark("first")]),
                    command("second", &[&fixture.mark("second"), "sleep:20000"]),
                ]),
            )
            .await;
        wait_for(&fixture.marker("second"));
        assert_eq!(
            fixture.stored_kinds(job),
            [
                "JOB_STARTED",
                "COMMAND_STARTED",
                "COMMAND_FINISHED",
                "COMMAND_STARTED"
            ]
        );

        let first = fixture.poll(job, 0).await;
        for _ in 0..3 {
            let again = fixture.poll(job, 0).await;
            assert_eq!(again, first, "the same cursor, the same durable events");
        }
        assert_eq!(first.job.state, VerificationJobState::Running);
        assert_eq!((first.next_seq, first.has_more), (4, false));
        let later = fixture.poll(job, 2).await;
        assert_eq!(later.events, first.events[2..]);
        let current = fixture.poll(job, 4).await;
        assert!(current.events.is_empty());
        assert_eq!((current.next_seq, current.has_more), (4, false));
        let page = fixture
            .runtime
            .managed_poll(fixture.workspace, job, 0, 1)
            .await
            .expect("page");
        assert_eq!(
            (page.events.len(), page.next_seq, page.has_more),
            (1, 1, true)
        );
        assert_eq!(count(&fixture.marker("first")), 1);
        assert_eq!(count(&fixture.marker("second")), 1);

        // Repeated managed use kept this daemon's own Job running.
        assert_eq!(
            fixture.poll(job, 0).await.job.state,
            VerificationJobState::Running
        );
        assert_eq!(
            fixture.runtime.managed_cancel(fixture.workspace, job).await,
            Ok(VerificationJobState::Cancelled)
        );
    });
}

/// Cancel ends the tree (no late marker), stores CANCELLED with one
/// terminal event, frees the slot and the raw reservation; a second
/// cancel changes nothing; an unknown Job is not found.
fn cancel_ends_the_tree_once() {
    block_on(async {
        let fixture = Fixture::new("cancel").await;
        let job = fixture
            .accepted(
                "cancel",
                slow_tree(&fixture.mark("started"), &fixture.mark("late")),
            )
            .await;
        wait_for(&fixture.marker("started"));
        assert_eq!(fixture.runtime.artifacts().accounting().reserved_count, 1);

        let began = Instant::now();
        assert_eq!(
            fixture.runtime.managed_cancel(fixture.workspace, job).await,
            Ok(VerificationJobState::Cancelled)
        );
        assert!(began.elapsed() < Duration::from_millis(LATE_MS));
        let done = fixture.poll(job, 0).await;
        assert_eq!(done.job.state, VerificationJobState::Cancelled);
        assert_eq!(
            shape(&done),
            [
                "JobStarted",
                "CommandStarted 0 tree",
                "JobCancelled CallerCancelled"
            ],
            "no synthetic CommandFinished for the cancelled command"
        );
        assert_eq!(fixture.runtime.artifacts().accounting().reserved_count, 0);

        assert_eq!(
            fixture.runtime.managed_cancel(fixture.workspace, job).await,
            Ok(VerificationJobState::Cancelled)
        );
        assert_eq!(terminal_events(&fixture, job), 1);
        assert_eq!(
            fixture
                .runtime
                .managed_cancel(fixture.workspace, VerificationJobId::generate())
                .await,
            Err(ManagedError::JobNotFound)
        );

        thread::sleep(Duration::from_millis(LATE_MS + 1000));
        assert!(!fixture.marker("late").exists(), "the tree outlived cancel");
        let next = fixture
            .accepted("after-cancel", batch(vec![command("next", &["exit:0"])]))
            .await;
        fixture.terminal(next).await;
    });
}

/// `Server::shutdown` stops the running Job, stores INTERRUPTED (not
/// CANCELLED) once, then purges the raw store and the endpoint. A new
/// daemon reruns nothing and adds no event.
fn clean_shutdown_interrupts_and_restart_reruns_nothing() {
    block_on(async {
        let fixture = Fixture::new("shutdown").await;
        let job = fixture
            .accepted(
                "shutdown",
                slow_tree(&fixture.mark("started"), &fixture.mark("late")),
            )
            .await;
        wait_for(&fixture.marker("started"));
        let artifacts = fixture.global_paths.runtime_root().join("artifacts");
        assert!(artifacts.exists());

        let Fixture {
            stop,
            server,
            home,
            root: _root,
            global_paths,
            endpoint,
            paths,
            workspace,
            connection,
            ..
        } = fixture;
        // Windows: an open client keeps its pipe instance -- and so the
        // pipe name -- claimed, refusing the restart's bind.
        drop(connection);
        stop.send(()).expect("server running");
        let server = server.await.expect("server task");
        let began = Instant::now();
        server.shutdown().await;
        assert!(began.elapsed() < Duration::from_millis(LATE_MS));
        drop(server);

        let mut store = VerificationJobStore::open(&paths.workspace_db).expect("db");
        let stored = store.get_by_id(job).expect("get").expect("job");
        assert_eq!(stored.state, VerificationJobState::Interrupted);
        let events = store.events_after(job, 0, 64).expect("events");
        let interrupted: Vec<_> = events
            .iter()
            .filter(|event| format!("{:?}", event.kind) == "JobInterrupted")
            .collect();
        assert_eq!(interrupted.len(), 1);
        assert_eq!(
            interrupted[0].payload_json,
            r#"{"v":1,"reason":"daemon_shutdown"}"#
        );
        assert!(
            !artifacts.join("verification").exists(),
            "raw store not purged"
        );
        #[cfg(unix)]
        assert!(!endpoint.socket_path.exists(), "socket left behind");
        assert!(!endpoint.lock_path.exists(), "lock left behind");
        // Nothing reconciles again in a later daemon.
        assert!(store.interrupt_all_running("{}").expect("noop").is_empty());
        drop(store);

        thread::sleep(Duration::from_millis(LATE_MS + 1000));
        assert!(
            !home.path().join("late").exists(),
            "the tree outlived shutdown"
        );

        let (_stop, _server, runtime) = serve(&global_paths).await;
        let poll = runtime
            .managed_poll(workspace, job, 0, 64)
            .await
            .expect("poll after restart");
        assert_eq!(poll.job.state, VerificationJobState::Interrupted);
        assert_eq!(poll.events.len(), events.len());
        assert_eq!(runtime.verification_runs(), 0, "a restart reran the Job");
        assert_eq!(count(&home.path().join("started")), 1);
    });
}

/// A RUNNING row no daemon runs (a crash's residue) becomes INTERRUPTED
/// on the Workspace's first managed use, with one event; later uses add
/// nothing, and nothing runs.
fn stale_running_is_interrupted_once_on_first_use() {
    block_on(async {
        let fixture = Fixture::new("stale").await;
        let stale = VerificationJobId::generate();
        VerificationJobStore::open_bound(fixture.workspace, &fixture.paths.workspace_db)
            .expect("bound")
            .create(&NewVerificationJob {
                uid: stale,
                idempotency_key: &IdempotencyKey::parse("crashed").expect("key"),
                request_fingerprint: RequestFingerprint([1; 32]),
                command_count: 1,
                started_payload_json: r#"{"v":1}"#,
            })
            .expect("residue");

        // Concurrent first uses reconcile once.
        let (poll, second, third) = tokio::join!(
            fixture.poll(stale, 0),
            fixture.poll(stale, 0),
            fixture.poll(stale, 0)
        );
        assert_eq!((&second, &third), (&poll, &poll));
        assert_eq!(poll.job.state, VerificationJobState::Interrupted);
        assert_eq!(shape(&poll), ["JobStarted", "JobInterrupted DaemonRestart"]);
        for _ in 0..3 {
            assert_eq!(fixture.poll(stale, 0).await, poll);
        }
        assert_eq!(
            fixture
                .runtime
                .managed_cancel(fixture.workspace, stale)
                .await,
            Ok(VerificationJobState::Interrupted)
        );
        assert_eq!(terminal_events(&fixture, stale), 1);
        assert_eq!(fixture.runtime.verification_runs(), 0);
    });
}

/// A raw capture's handle is Available while the store holds it -- in
/// CommandFinished and JobFinished alike -- and Unavailable once it is
/// gone; the Job and its outcome never change.
fn raw_availability_follows_the_store_not_the_db() {
    block_on(async {
        let fixture = Fixture::new("raw").await;
        let job = fixture
            .accepted(
                "raw",
                batch(vec![capturing("raw", &["out:1000"], true, None)]),
            )
            .await;
        let done = fixture.terminal(job).await;
        let CaptureProgress::Captured {
            raw: RawAvailabilityWire::Available(progress),
            ..
        } = &command_finished(&done, 0).capture
        else {
            panic!("expected an available raw artifact")
        };
        let CommandCaptureWire::Captured {
            raw: RawAvailabilityWire::Available(terminal),
            ..
        } = &job_finished(&done)[0].capture
        else {
            panic!("expected an available raw artifact")
        };
        assert_eq!(progress, terminal);
        assert_eq!(terminal.stdout.observed_bytes, 1000);
        let store = fixture.runtime.artifacts();
        let bytes = store
            .read_part(
                &terminal.handle,
                ArtifactStream::Stdout,
                ArtifactPart::Head,
                0,
                4096,
            )
            .expect("readable");
        assert_eq!(bytes, vec![b'o'; 1000]);

        store.remove(&terminal.handle);
        let after = fixture.poll(job, 0).await;
        assert_eq!(after.job, done.job, "the Job is unchanged");
        assert!(matches!(
            command_finished(&after, 0).capture,
            CaptureProgress::Captured {
                raw: RawAvailabilityWire::Unavailable,
                ..
            }
        ));
        let result = &job_finished(&after)[0];
        assert_eq!(result.outcome, VerificationOutcomeWire::Passed);
        assert!(matches!(
            result.capture,
            CommandCaptureWire::Captured {
                raw: RawAvailabilityWire::Unavailable,
                ..
            }
        ));
    });
}

/// CommandFinished carries diagnostic counts only; JobFinished carries
/// the #53 items, once.
fn diagnostic_items_are_stored_once_in_job_finished() {
    block_on(async {
        let fixture = Fixture::new("diagnostics").await;
        let job = fixture
            .accepted(
                "diagnostics",
                batch(vec![capturing(
                    "lint",
                    &["line:src/app.rs:1:2: error: unique-boom-message"],
                    false,
                    Some(DiagnosticFormatWire::PathLineColumn),
                )]),
            )
            .await;
        let done = fixture.terminal(job).await;
        let CaptureProgress::Captured {
            diagnostics: Some(counts),
            raw,
            ..
        } = &command_finished(&done, 0).capture
        else {
            panic!("expected diagnostics counts")
        };
        assert_eq!((counts.observed, counts.retained), (1, 1));
        assert_eq!(*raw, RawAvailabilityWire::NotRequested);
        let CommandCaptureWire::Captured {
            diagnostics: Some(summary),
            ..
        } = &job_finished(&done)[0].capture
        else {
            panic!("expected diagnostics")
        };
        assert_eq!(summary.items.len(), 1);
        assert_eq!(summary.items[0].message, "unique-boom-message");
        assert_eq!(summary.delivery_omitted, 0);

        let stored: i64 = fixture
            .db()
            .query_row(
                "SELECT COUNT(*) FROM verification_job_event \
                 WHERE payload_json LIKE '%unique-boom-message%'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(stored, 1, "a diagnostic body is stored exactly once");
    });
}

/// A COMMAND_STARTED that cannot be stored spawns nothing; a
/// COMMAND_FINISHED that cannot be stored runs nothing later and drops
/// that command's fresh raw artifact. Both end INTERNAL_ERROR and free
/// their slot.
fn a_failed_event_write_stops_the_batch() {
    block_on(async {
        let fixture = Fixture::new("event-failure").await;
        let inject = |kind: &str| {
            rusqlite::Connection::open(&fixture.paths.workspace_db)
                .expect("db")
                .execute_batch(&format!(
                    "DROP TRIGGER IF EXISTS inject; \
                     CREATE TRIGGER inject BEFORE INSERT ON verification_job_event \
                     WHEN NEW.kind = '{kind}' BEGIN SELECT RAISE(ABORT, 'injected'); END;"
                ))
                .expect("trigger");
        };

        inject("COMMAND_STARTED");
        let job = fixture
            .accepted(
                "started-fails",
                batch(vec![command("never", &[&fixture.mark("never")])]),
            )
            .await;
        let done = fixture.terminal(job).await;
        assert_eq!(done.job.state, VerificationJobState::InternalError);
        assert_eq!(
            shape(&done),
            ["JobStarted", "JobInternalError EventPersistence"]
        );
        assert!(
            !fixture.marker("never").exists(),
            "spawned without its event"
        );

        inject("COMMAND_FINISHED");
        let before = fixture.runtime.artifacts().accounting();
        let job = fixture
            .accepted(
                "finished-fails",
                batch(vec![
                    capturing("ran", &[&fixture.mark("ran"), "out:10"], true, None),
                    command("later", &[&fixture.mark("later")]),
                ]),
            )
            .await;
        let done = fixture.terminal(job).await;
        assert_eq!(done.job.state, VerificationJobState::InternalError);
        assert_eq!(
            shape(&done),
            [
                "JobStarted",
                "CommandStarted 0 ran",
                "JobInternalError EventPersistence"
            ]
        );
        assert_eq!(count(&fixture.marker("ran")), 1);
        assert!(!fixture.marker("later").exists(), "ran past a lost event");
        assert_eq!(
            fixture.runtime.artifacts().accounting(),
            before,
            "the unrecorded artifact was kept"
        );

        rusqlite::Connection::open(&fixture.paths.workspace_db)
            .expect("db")
            .execute_batch("DROP TRIGGER inject;")
            .expect("drop trigger");
        let next = fixture
            .accepted("after-failure", batch(vec![command("next", &["exit:0"])]))
            .await;
        assert_eq!(
            fixture.terminal(next).await.job.state,
            VerificationJobState::Finished
        );
    });
}

/// argv, env and cwd reach the command but never workspace.db (or its
/// WAL/SHM); labels do.
fn request_values_are_never_stored() {
    block_on(async {
        let fixture = Fixture::new("secrets").await;
        let cwd = fixture.root.path().join("SECRET_CWD_MARKER");
        fs::create_dir_all(&cwd).expect("cwd");
        let mut secret = command(
            "secret-label",
            &[&fixture.mark("ran"), "line:SECRET_ARG_MARKER"],
        );
        secret.cwd = Some("SECRET_CWD_MARKER".to_owned());
        secret.env = vec![("SECRET_ENV".to_owned(), "SECRET_ENV_MARKER".to_owned())];
        secret.capture = Some(VerificationCaptureWire {
            raw: true,
            diagnostics: Some(DiagnosticFormatWire::PathLineColumn),
        });
        let job = fixture.accepted("secrets", batch(vec![secret])).await;
        assert_eq!(
            fixture.terminal(job).await.job.state,
            VerificationJobState::Finished
        );
        assert_eq!(count(&fixture.marker("ran")), 1);

        let mut stored = Vec::new();
        for suffix in ["", "-wal", "-shm"] {
            let path = PathBuf::from(format!("{}{suffix}", fixture.paths.workspace_db.display()));
            stored.extend(fs::read(path).unwrap_or_default());
        }
        let text = String::from_utf8_lossy(&stored);
        for marker in [
            "SECRET_ARG_MARKER",
            "SECRET_ENV_MARKER",
            "SECRET_CWD_MARKER",
        ] {
            assert!(!text.contains(marker), "{marker} reached workspace.db");
        }
        assert!(text.contains("secret-label"), "the label is a stored fact");
        fs::remove_dir_all(cwd).expect("cleanup");
    });
}

/// A long managed command never holds the Workspace worker, and the Job's
/// workspace.db writes are no source change.
fn the_worker_stays_free_and_git_stays_clean() {
    block_on(async {
        let fixture = Fixture::new("worker").await;
        let job = fixture
            .accepted(
                "worker",
                batch(vec![command(
                    "long",
                    &[&fixture.mark("long"), "sleep:20000"],
                )]),
            )
            .await;
        wait_for(&fixture.marker("long"));
        let stats = tokio::time::timeout(
            Duration::from_secs(5),
            fixture.runtime.lifecycle_stats(fixture.workspace),
        )
        .await
        .expect("the worker answered while the Job ran");
        assert!(stats.is_some_and(|stats| stats.activated));
        assert_eq!(
            fixture.runtime.managed_cancel(fixture.workspace, job).await,
            Ok(VerificationJobState::Cancelled)
        );

        let finished = fixture
            .accepted("worker-2", batch(vec![command("short", &["exit:0"])]))
            .await;
        fixture.terminal(finished).await;
        let status = git_status::observe(&fixture.paths.workspace_root).expect("git status");
        assert_eq!(
            status.observation,
            brainprint_engine::git_observation::GitObservation::Clean,
            "managed Job writes dirtied the Workspace"
        );
    });
}

fn invalid_requests_create_nothing() {
    block_on(async {
        let fixture = Fixture::new("invalid").await;
        let never = fixture.mark("never");
        let fine = || batch(vec![command("ok", &[&never])]);
        assert_eq!(
            fixture.start("bad key", fine()).await,
            Err(ManagedError::InvalidIdempotencyKey)
        );
        assert!(matches!(
            fixture.start("empty", batch(Vec::new())).await,
            Err(ManagedError::InvalidVerification(_))
        ));
        let mut empty_capture = command("cap", &[&never]);
        empty_capture.capture = Some(VerificationCaptureWire {
            raw: false,
            diagnostics: None,
        });
        assert!(matches!(
            fixture.start("cap", batch(vec![empty_capture])).await,
            Err(ManagedError::InvalidVerification(_))
        ));
        let job = VerificationJobId::generate();
        for limit in [0, 65] {
            assert_eq!(
                fixture
                    .runtime
                    .managed_poll(fixture.workspace, job, 0, limit)
                    .await,
                Err(ManagedError::InvalidLimit)
            );
        }
        assert_eq!(
            fixture
                .runtime
                .managed_poll(fixture.workspace, job, 0, 1)
                .await,
            Err(ManagedError::JobNotFound)
        );
        let unregistered = WorkspaceId::from_bytes([7; 16]);
        assert!(matches!(
            fixture
                .runtime
                .managed_start(unregistered, "k", fine())
                .await,
            Err(ManagedError::Workspace(_))
        ));
        let rows: i64 = fixture
            .db()
            .query_row("SELECT COUNT(*) FROM verification_job", [], |row| {
                row.get(0)
            })
            .expect("count");
        assert_eq!(rows, 0);
        assert_eq!(fixture.runtime.verification_runs(), 0);
        assert!(!fixture.marker("never").exists());
    });
}

fn protocol_is_8_and_workspace_schema_6() {
    assert_eq!(PROTOCOL_VERSION, 8);
    assert_eq!(
        brainprint_engine::schema::workspace::WORKSPACE_MIGRATIONS.len(),
        6
    );
    let _ = EndReason::DaemonRestart;
}
