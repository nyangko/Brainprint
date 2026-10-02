//! #52 step 3 acceptance: a Work Result's `verification` through the real
//! daemon protocol and the real CLI.
//!
//! `harness = false`: this test binary is also the portable helper the
//! commands run (no shell), so the same tests hold on macOS, Ubuntu and
//! Windows. Helper ops, in order: `exit:<code>`, `sleep:<ms>`,
//! `mark:<path>` (append one byte: the file length counts runs),
//! `await:<path>` (until it exists), `note:<text>` (no-op payload),
//! `spawn [ ops… ]` (a child left running).
//!
//! Every refusal is checked for "nothing ran" (no marker) and "nothing
//! written" (row counts of every workspace.db work table).

use std::{
    env, fs,
    future::Future,
    io::Write as _,
    path::{Path, PathBuf},
    process::{self, Child, Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use brainprint_core::{
    PROTOCOL_VERSION, WorkItemId,
    protocol::{
        self, ClientConnection, HandshakeRequest, HandshakeResponse, InitRequest, InitResponse,
        Request, Response,
        query::{
            DirtyObservationWire, WorkItemSourceKindWire, WorkItemStatusWire, WorkResultStatusWire,
            WorkspaceSelectorWire,
        },
        work::*,
    },
};
use brainprint_daemon::{query::DaemonQueryRuntime, runtime_paths, server::Server};
use brainprint_engine::{
    git_observation::{self, GitEntryStatus, GitObservation},
    git_status,
    paths::{GlobalPaths, WorkspacePaths},
};

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
            "partial_verification_is_recorded_with_its_results",
            partial_verification_is_recorded_with_its_results,
        ),
        (
            "a_failed_verification_keeps_the_callers_complete",
            a_failed_verification_keeps_the_callers_complete,
        ),
        ("invalid_requests_run_nothing", invalid_requests_run_nothing),
        (
            "concurrent_verification_is_busy_not_queued",
            concurrent_verification_is_busy_not_queued,
        ),
        (
            "a_client_disconnect_cancels_the_tree_and_records_nothing",
            a_client_disconnect_cancels_the_tree_and_records_nothing,
        ),
        (
            "a_daemon_shutdown_cancels_and_a_restart_reruns_nothing",
            a_daemon_shutdown_cancels_and_a_restart_reruns_nothing,
        ),
        (
            "a_storage_failure_after_the_run_keeps_the_results",
            a_storage_failure_after_the_run_keeps_the_results,
        ),
        (
            "a_write_race_after_the_run_is_invalid_transition_with_results",
            a_write_race_after_the_run_is_invalid_transition_with_results,
        ),
        (
            "observe_runs_after_verification_and_sees_its_files",
            observe_runs_after_verification_and_sees_its_files,
        ),
        (
            "an_observe_failure_after_the_run_keeps_the_results",
            an_observe_failure_after_the_run_keeps_the_results,
        ),
        (
            "a_result_without_verification_starts_no_run",
            a_result_without_verification_starts_no_run,
        ),
        (
            "the_cli_runs_verification_and_maps_exit_codes",
            the_cli_runs_verification_and_maps_exit_codes,
        ),
        ("protocol_version_is_11", protocol_version_is_11),
        (
            "a_v10_client_is_refused_by_a_v11_daemon",
            a_v10_client_is_refused_by_a_v11_daemon,
        ),
        (
            "a_v11_client_stops_at_a_v10_daemon",
            a_v11_client_stops_at_a_v10_daemon,
        ),
        (
            "mcp_tools_list_is_byte_identical",
            mcp_tools_list_is_byte_identical,
        ),
        (
            "verification_finds_no_command_and_mcp_exposes_none",
            verification_finds_no_command_and_mcp_exposes_none,
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
            "await" => {
                let deadline = Instant::now() + Duration::from_secs(60);
                while !Path::new(value).exists() {
                    assert!(Instant::now() < deadline, "never released");
                    thread::sleep(Duration::from_millis(10));
                }
            }
            "note" => {}
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

fn verification(commands: Vec<VerificationCommandWire>) -> Option<VerificationWire> {
    Some(VerificationWire { commands })
}

fn result_input(
    work_item: WorkItemId,
    outcome: WorkOutcomeWire,
    verification: Option<VerificationWire>,
) -> WorkResultInputWire {
    WorkResultInputWire {
        work_item,
        outcome,
        summary: "done".to_owned(),
        commit_id: None,
        verification_summary: None,
        verification,
        verification_job: None,
        git: GitObservationWire::Unknown,
        change_set: None,
    }
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
            "brainprint-i6-task3-{label}-{}-{sequence}",
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

fn workspace_tree(root: &Path, is_git: bool) {
    fs::create_dir_all(root.join("src")).expect("src");
    fs::write(root.join("src/app.rs"), "pub fn app() {}\n").expect("app.rs");
    if is_git {
        git(root, &["init", "-q"]);
        git(root, &["config", "core.autocrlf", "false"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "init"]);
    }
}

async fn send(connection: &mut ClientConnection, request: &Request) -> Response {
    protocol::framing::write_message(connection, request)
        .await
        .expect("write should succeed");
    protocol::framing::read_message(connection)
        .await
        .expect("read should succeed")
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
        &Request::Handshake(HandshakeRequest {
            protocol_version,
            client_kind: "i6-task3-test".to_owned(),
        }),
    )
    .await
}

async fn open_connection(endpoint: &runtime_paths::RuntimeEndpoint) -> ClientConnection {
    let mut connection = connect(endpoint).await;
    assert!(matches!(
        handshake(&mut connection, PROTOCOL_VERSION).await,
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

fn work_request(paths: &WorkspacePaths, operation: WorkOperationWire) -> Request {
    Request::Work(WorkRequest {
        workspace: WorkspaceSelectorWire::Locator {
            path: paths.workspace_root.to_string_lossy().into_owned(),
        },
        operation,
    })
}

async fn work(
    connection: &mut ClientConnection,
    paths: &WorkspacePaths,
    operation: WorkOperationWire,
) -> WorkResponse {
    match send(connection, &work_request(paths, operation)).await {
        Response::Work(response) => response,
        other => panic!("expected Work, got {other:?}"),
    }
}

async fn start(connection: &mut ClientConnection, paths: &WorkspacePaths) -> WorkItemId {
    let operation = WorkOperationWire::Start(WorkStartWire {
        work_item: WorkStartItemWire::New(NewWorkItemWire {
            source_kind: WorkItemSourceKindWire::Issue,
            source_ref: Some("#52".to_owned()),
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

/// Row counts of every work table: the "nothing written" witness.
fn counts(paths: &WorkspacePaths) -> Vec<i64> {
    let connection = rusqlite::Connection::open_with_flags(
        &paths.workspace_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("workspace.db");
    [
        "work_item",
        "working_state",
        "work_resource",
        "work_result",
        "work_handoff",
    ]
    .into_iter()
    .map(|table| {
        connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count")
    })
    .collect()
}

fn stored_summary(paths: &WorkspacePaths, work_item: WorkItemId) -> Option<String> {
    rusqlite::Connection::open_with_flags(
        &paths.workspace_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("workspace.db")
    .query_row(
        "SELECT r.verification_summary FROM work_result r
         JOIN work_item i ON i.id = r.work_item_id WHERE i.uid = ?1",
        [work_item.to_bytes().to_vec()],
        |row| row.get(0),
    )
    .expect("stored result")
}

/// An in-process daemon with one initialized Workspace.
struct Fixture {
    home: TestDir,
    _root: TestDir,
    endpoint: runtime_paths::RuntimeEndpoint,
    connection: ClientConnection,
    paths: WorkspacePaths,
    runtime: Arc<DaemonQueryRuntime>,
    _server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new(label: &str, is_git: bool) -> Self {
        let home = TestDir::create(&format!("{label}-home"));
        let root = TestDir::create(&format!("{label}-root"));
        workspace_tree(root.path(), is_git);
        let global_paths = GlobalPaths::from_home(home.path());
        let mut server = Server::bind(&global_paths).await.expect("bind");
        let runtime = server.query_runtime();
        let endpoint = runtime_paths::resolve(&global_paths);
        let server_task = tokio::spawn(async move {
            let _ = server.serve().await;
        });
        let mut connection = open_connection(&endpoint).await;
        let paths = init(&mut connection, root.path()).await;
        Self {
            home,
            _root: root,
            endpoint,
            connection,
            paths,
            runtime,
            _server: server_task,
        }
    }

    /// A marker path outside the Workspace (its writes never touch Git).
    fn marker(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    fn mark(&self, name: &str) -> String {
        format!("mark:{}", self.marker(name).display())
    }

    async fn work(&mut self, operation: WorkOperationWire) -> WorkResponse {
        work(&mut self.connection, &self.paths, operation).await
    }

    async fn result(&mut self, input: WorkResultInputWire) -> WorkResponse {
        self.work(WorkOperationWire::Result(input)).await
    }

    async fn started(&mut self) -> WorkItemId {
        start(&mut self.connection, &self.paths).await
    }

    fn counts(&self) -> Vec<i64> {
        counts(&self.paths)
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
        WorkResponse::Failed(failure) => {
            assert_eq!(failure.created_work_item, None);
            failure
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

fn outcomes(results: &[CommandResultWire]) -> Vec<(&str, VerificationOutcomeWire)> {
    results
        .iter()
        .map(|result| (result.label.as_str(), result.outcome))
        .collect()
}

// ---------------------------------------------------------------- record

fn partial_verification_is_recorded_with_its_results() {
    block_on(async {
        let mut fixture = Fixture::new("partial", false).await;
        let work_item = fixture.started().await;
        let mark = fixture.mark("ran");
        let response = fixture
            .result(result_input(
                work_item,
                WorkOutcomeWire::Partial,
                verification(vec![
                    command("build", &[&mark]),
                    command("test", &["exit:0"]),
                ]),
            ))
            .await;
        let recorded = recorded(response);
        assert_eq!(recorded.status, WorkItemStatusWire::Active);
        assert_eq!(recorded.result.result_status, WorkResultStatusWire::Partial);
        let results = recorded.verification.expect("the verification ran");
        assert_eq!(
            outcomes(&results),
            [
                ("build", VerificationOutcomeWire::Passed),
                ("test", VerificationOutcomeWire::Passed)
            ]
        );
        let summary = recorded.result.verification_summary.expect("summary");
        assert!(summary.starts_with("build: passed "), "{summary}");
        assert!(summary.contains("; test: passed "), "{summary}");
        assert_eq!(stored_summary(&fixture.paths, work_item), Some(summary));
        assert_eq!(count(&fixture.marker("ran")), 1);
        assert_eq!(fixture.runtime.verification_runs(), 1);
    });
}

/// A failed command is a fact: the caller's `Complete` still completes.
fn a_failed_verification_keeps_the_callers_complete() {
    block_on(async {
        let mut fixture = Fixture::new("complete", false).await;
        let work_item = fixture.started().await;
        let mark = fixture.mark("after");
        let recorded = recorded(
            fixture
                .result(result_input(
                    work_item,
                    WorkOutcomeWire::Complete,
                    verification(vec![
                        command("test", &["exit:3"]),
                        command("after", &[&mark]),
                    ]),
                ))
                .await,
        );
        assert_eq!(recorded.status, WorkItemStatusWire::Completed);
        assert_eq!(
            recorded.result.result_status,
            WorkResultStatusWire::Completed
        );
        assert_eq!(
            outcomes(&recorded.verification.expect("ran")),
            [
                ("test", VerificationOutcomeWire::Failed { exit_code: 3 }),
                ("after", VerificationOutcomeWire::Skipped)
            ]
        );
        let summary = recorded.result.verification_summary.expect("summary");
        assert!(summary.starts_with("test: failed exit=3 "), "{summary}");
        assert!(summary.ends_with("; after: skipped"), "{summary}");
        assert!(!fixture.marker("after").exists());
    });
}

// --------------------------------------------------------------- refusal

type Expected = dyn Fn(&WorkErrorWire) -> bool;

fn invalid_requests_run_nothing() {
    block_on(async {
        let mut fixture = Fixture::new("invalid", false).await;
        let active = fixture.started().await;
        let terminal = fixture.started().await;
        let mut finish = result_input(terminal, WorkOutcomeWire::Complete, None);
        finish.verification_summary = Some("given".to_owned());
        recorded(fixture.result(finish).await);
        let mark = fixture.mark("never");
        let ok = || command("ok", &[&mark]);
        let with = |commands: Vec<VerificationCommandWire>| {
            result_input(active, WorkOutcomeWire::Partial, verification(commands))
        };
        let escape = || GitEntryWire {
            status: GitEntryStatusWire::Modified,
            path: "../escape".to_owned(),
            old_path: None,
        };

        let mut both = with(vec![ok()]);
        both.verification_summary = Some("given".to_owned());
        let mut bad_label = ok();
        bad_label.label = "bad label!".to_owned();
        let mut empty_argv = ok();
        empty_argv.argv.clear();
        let mut zero_timeout = ok();
        zero_timeout.timeout_secs = 0;
        let mut long = ok();
        long.timeout_secs = 3000;
        let mut longer = command("longer", &[&mark]);
        longer.timeout_secs = 3000;
        let mut outside = ok();
        outside.cwd = Some("../".to_owned());
        let mut absolute = ok();
        absolute.cwd = Some(fixture.paths.workspace_root.to_string_lossy().into_owned());
        let mut missing = ok();
        missing.cwd = Some("no-such-dir".to_owned());
        let mut bad_git = with(vec![ok()]);
        bad_git.git = GitObservationWire::Dirty {
            entries: vec![escape()],
        };
        let mut bad_change_set = with(vec![ok()]);
        bad_change_set.change_set = Some(vec![escape()]);

        let invalid =
            |error: &WorkErrorWire| matches!(error, WorkErrorWire::InvalidObservation { .. });
        let transition =
            |error: &WorkErrorWire| matches!(error, WorkErrorWire::InvalidTransition { .. });
        let cases: Vec<(&str, WorkResultInputWire, &Expected)> = vec![
            ("verification and summary", both, &invalid),
            ("no commands", with(Vec::new()), &invalid),
            ("label", with(vec![bad_label]), &invalid),
            ("duplicate label", with(vec![ok(), ok()]), &invalid),
            ("argv", with(vec![empty_argv]), &invalid),
            ("timeout", with(vec![zero_timeout]), &invalid),
            ("total timeout", with(vec![long, longer]), &invalid),
            ("cwd outside", with(vec![outside]), &invalid),
            ("cwd absolute", with(vec![absolute]), &invalid),
            ("cwd missing", with(vec![missing]), &invalid),
            ("git entries", bad_git, &invalid),
            ("change set", bad_change_set, &invalid),
            (
                "unknown WorkItem",
                result_input(
                    WorkItemId::generate(),
                    WorkOutcomeWire::Partial,
                    verification(vec![ok()]),
                ),
                &|error| matches!(error, WorkErrorWire::WorkItemNotFound { .. }),
            ),
            (
                "terminal Complete",
                result_input(
                    terminal,
                    WorkOutcomeWire::Complete,
                    verification(vec![ok()]),
                ),
                &transition,
            ),
            (
                "terminal Abandon",
                result_input(terminal, WorkOutcomeWire::Abandon, verification(vec![ok()])),
                &transition,
            ),
            (
                "terminal Partial",
                result_input(terminal, WorkOutcomeWire::Partial, verification(vec![ok()])),
                &|error| !matches!(error, WorkErrorWire::Internal { .. }),
            ),
        ];
        let before = fixture.counts();
        let runs = fixture.runtime.verification_runs();
        for (name, input, expected) in cases {
            let failure = failed(fixture.result(input).await);
            assert!(expected(&failure.error), "{name}: {:?}", failure.error);
            assert_eq!(failure.verification, None, "{name}");
        }
        assert!(!fixture.marker("never").exists(), "a refused command ran");
        assert_eq!(fixture.counts(), before);
        assert_eq!(fixture.runtime.verification_runs(), runs);
    });
}

// ----------------------------------------------------------- concurrency

fn concurrent_verification_is_busy_not_queued() {
    block_on(async {
        let mut fixture = Fixture::new("busy", false).await;
        let first = fixture.started().await;
        let second = fixture.started().await;
        let started = fixture.mark("started");
        // Held between `started` and `ran` until the test releases it, so
        // every check below happens while the first run is RUNNING.
        let release = fixture.marker("release");
        let held = format!("await:{}", release.display());
        let slow = verification(vec![command(
            "slow",
            &[&started, &held, &fixture.mark("ran")],
        )]);
        let mut other = open_connection(&fixture.endpoint).await;
        let paths = fixture.paths.clone();
        let running = tokio::spawn(async move {
            let response = work(
                &mut other,
                &paths,
                WorkOperationWire::Result(result_input(first, WorkOutcomeWire::Partial, slow)),
            )
            .await;
            (other, response)
        });
        wait_for(&fixture.marker("started"));
        let before = fixture.counts();

        let never = fixture.mark("never");
        for work_item in [first, second] {
            let failure = failed(
                fixture
                    .result(result_input(
                        work_item,
                        WorkOutcomeWire::Partial,
                        verification(vec![command("again", &[&never])]),
                    ))
                    .await,
            );
            assert_eq!(failure.error, WorkErrorWire::VerificationBusy);
            assert_eq!(failure.verification, None);
        }
        assert_eq!(fixture.counts(), before);

        // Another Workspace of the same daemon runs meanwhile.
        let other_root = TestDir::create("busy-other-root");
        workspace_tree(other_root.path(), false);
        let mut third = open_connection(&fixture.endpoint).await;
        let other_paths = init(&mut third, other_root.path()).await;
        let elsewhere = start(&mut third, &other_paths).await;
        let parallel = recorded(
            work(
                &mut third,
                &other_paths,
                WorkOperationWire::Result(result_input(
                    elsewhere,
                    WorkOutcomeWire::Partial,
                    verification(vec![command("fast", &["exit:0"])]),
                )),
            )
            .await,
        );
        assert!(parallel.verification.is_some());
        assert!(
            !fixture.marker("ran").exists(),
            "ran beside the first run, not after it"
        );

        fs::write(&release, b"").expect("release");
        let (_other, response) = running.await.expect("first request");
        recorded(response);
        assert!(!fixture.marker("never").exists());
        assert_eq!(count(&fixture.marker("started")), 1);
        assert_eq!(count(&fixture.marker("ran")), 1);
        assert_eq!(fixture.runtime.verification_runs(), 2);
    });
}

// ---------------------------------------------------------- cancellation

/// A long command that leaves a child: both would write `late`.
fn slow_tree(started: &str, late: &str) -> Option<VerificationWire> {
    let sleep = format!("sleep:{LATE_MS}");
    verification(vec![command(
        "slow",
        &[started, "spawn", "[", &sleep, late, "]", &sleep, late],
    )])
}

fn a_client_disconnect_cancels_the_tree_and_records_nothing() {
    block_on(async {
        let mut fixture = Fixture::new("disconnect", false).await;
        let work_item = fixture.started().await;
        let before = fixture.counts();
        let mut other = open_connection(&fixture.endpoint).await;
        let request = work_request(
            &fixture.paths,
            WorkOperationWire::Result(result_input(
                work_item,
                WorkOutcomeWire::Partial,
                slow_tree(&fixture.mark("started"), &fixture.mark("late")),
            )),
        );
        protocol::framing::write_message(&mut other, &request)
            .await
            .expect("write");
        wait_for(&fixture.marker("started"));
        drop(other);

        tokio::time::sleep(Duration::from_millis(LATE_MS + 2500)).await;
        assert!(
            !fixture.marker("late").exists(),
            "the tree outlived the client"
        );
        assert_eq!(fixture.counts(), before);

        // The Workspace's slot was released with the cancelled tree.
        let after = recorded(
            fixture
                .result(result_input(
                    work_item,
                    WorkOutcomeWire::Partial,
                    verification(vec![command("next", &["exit:0"])]),
                ))
                .await,
        );
        assert!(after.verification.is_some());
    });
}

/// A normal shutdown drops every connection's future, which cancels the
/// run; the restarted daemon has nothing to resume.
fn a_daemon_shutdown_cancels_and_a_restart_reruns_nothing() {
    let home = TestDir::create("restart-home");
    let root = TestDir::create("restart-root");
    workspace_tree(root.path(), false);
    let global_paths = GlobalPaths::from_home(home.path());
    let endpoint = runtime_paths::resolve(&global_paths);
    let started = home.path().join("started");
    let late = home.path().join("late");
    let serve = |runtime: &tokio::runtime::Runtime| {
        let mut server = runtime.block_on(Server::bind(&global_paths)).expect("bind");
        let query_runtime = server.query_runtime();
        runtime.spawn(async move {
            let _ = server.serve().await;
        });
        query_runtime
    };
    let daemon = tokio::runtime::Runtime::new().expect("runtime");
    serve(&daemon);

    let client = tokio::runtime::Runtime::new().expect("runtime");
    let (mut connection, paths, before) = client.block_on(async {
        let mut connection = open_connection(&endpoint).await;
        let paths = init(&mut connection, root.path()).await;
        let work_item = start(&mut connection, &paths).await;
        let before = counts(&paths);
        let request = work_request(
            &paths,
            WorkOperationWire::Result(result_input(
                work_item,
                WorkOutcomeWire::Partial,
                slow_tree(
                    &format!("mark:{}", started.display()),
                    &format!("mark:{}", late.display()),
                ),
            )),
        );
        protocol::framing::write_message(&mut connection, &request)
            .await
            .expect("write");
        (connection, paths, before)
    });
    wait_for(&started);

    let shutdown = Instant::now();
    drop(daemon);
    assert!(shutdown.elapsed() < Duration::from_secs(LATE_MS / 1000 + 2));
    let reply: std::io::Result<Response> =
        client.block_on(protocol::framing::read_message(&mut connection));
    assert!(reply.is_err(), "no answer after shutdown: {reply:?}");
    thread::sleep(Duration::from_millis(LATE_MS + 1000));
    assert!(!late.exists(), "the tree outlived the daemon");
    assert_eq!(counts(&paths), before);

    let restarted = tokio::runtime::Runtime::new().expect("runtime");
    let query_runtime = serve(&restarted);
    client.block_on(async {
        let mut connection = open_connection(&endpoint).await;
        init(&mut connection, root.path()).await;
    });
    thread::sleep(Duration::from_millis(1000));
    assert_eq!(query_runtime.verification_runs(), 0);
    assert_eq!(count(&started), 1, "nothing was run again");
    assert!(!late.exists());
    assert_eq!(counts(&paths), before);
}

// ------------------------------------------------------- after-run faults

/// A storage failure after the run. The seam is a test-only trigger that
/// refuses the result row: the preflight only reads, so the commands run,
/// and the write after them fails and rolls back. (A read-only file does
/// not work here: SQLite reuses the daemon's already-open read-write
/// descriptor for the same file.)
fn a_storage_failure_after_the_run_keeps_the_results() {
    block_on(async {
        let mut fixture = Fixture::new("storage", false).await;
        let work_item = fixture.started().await;
        let before = fixture.counts();
        let db = rusqlite::Connection::open(&fixture.paths.workspace_db).expect("workspace.db");
        db.execute_batch(
            "CREATE TRIGGER i6_task3_seam BEFORE INSERT ON work_result
             BEGIN SELECT RAISE(ABORT, 'i6_task3 storage seam'); END;",
        )
        .expect("seam");
        let response = fixture
            .result(result_input(
                work_item,
                WorkOutcomeWire::Partial,
                verification(vec![command("ran", &[&fixture.mark("ran")])]),
            ))
            .await;
        db.execute_batch("DROP TRIGGER i6_task3_seam")
            .expect("drop seam");

        let failure = failed(response);
        assert!(
            matches!(failure.error, WorkErrorWire::Internal { .. }),
            "{failure:?}"
        );
        assert_eq!(
            outcomes(&failure.verification.expect("the command ran")),
            [("ran", VerificationOutcomeWire::Passed)]
        );
        assert_eq!(count(&fixture.marker("ran")), 1);
        assert_eq!(fixture.counts(), before);
    });
}

/// The preflight passed, the WorkItem finished while the commands ran:
/// the write's own check refuses, and the results still come back.
fn a_write_race_after_the_run_is_invalid_transition_with_results() {
    block_on(async {
        let mut fixture = Fixture::new("race", false).await;
        let work_item = fixture.started().await;
        let mut other = open_connection(&fixture.endpoint).await;
        let paths = fixture.paths.clone();
        let slow = verification(vec![command(
            "slow",
            &[&fixture.mark("started"), "sleep:2000"],
        )]);
        let running = tokio::spawn(async move {
            work(
                &mut other,
                &paths,
                WorkOperationWire::Result(result_input(work_item, WorkOutcomeWire::Complete, slow)),
            )
            .await
        });
        wait_for(&fixture.marker("started"));
        let mut finish = result_input(work_item, WorkOutcomeWire::Complete, None);
        finish.verification_summary = Some("given".to_owned());
        recorded(fixture.result(finish).await);
        let after = fixture.counts();

        let failure = failed(running.await.expect("request"));
        assert!(
            matches!(failure.error, WorkErrorWire::InvalidTransition { .. }),
            "{failure:?}"
        );
        assert_eq!(
            outcomes(&failure.verification.expect("the command ran")),
            [("slow", VerificationOutcomeWire::Passed)]
        );
        assert_eq!(fixture.counts(), after);
        assert_eq!(
            stored_summary(&fixture.paths, work_item).as_deref(),
            Some("given")
        );
    });
}

// ------------------------------------------------------------- + Observe

fn observe_runs_after_verification_and_sees_its_files() {
    block_on(async {
        let mut fixture = Fixture::new("observe", true).await;
        let work_item = fixture.started().await;
        let root = fixture.paths.workspace_root.clone();
        assert_eq!(
            git_status::observe(&root).expect("observe").observation,
            GitObservation::Clean
        );
        let generate = format!("mark:{}", root.join("generated.txt").display());
        let mut input = result_input(
            work_item,
            WorkOutcomeWire::Partial,
            verification(vec![command("gen", &[&generate])]),
        );
        input.git = GitObservationWire::Observe;
        let recorded = recorded(fixture.result(input).await);

        let GitObservation::Dirty(entries) =
            git_status::observe(&root).expect("observe").observation
        else {
            panic!("the command left a file")
        };
        assert!(
            entries
                .iter()
                .any(|entry| entry.path == "generated.txt"
                    && entry.status == GitEntryStatus::Untracked),
            "{entries:?}"
        );
        assert_eq!(
            recorded.result.remaining_dirty,
            DirtyObservationWire::Dirty {
                fingerprint: git_observation::fingerprint(&entries),
            }
        );
    });
}

fn an_observe_failure_after_the_run_keeps_the_results() {
    block_on(async {
        let mut fixture = Fixture::new("observe-fail", false).await;
        let work_item = fixture.started().await;
        let before = fixture.counts();
        let mut input = result_input(
            work_item,
            WorkOutcomeWire::Partial,
            verification(vec![command("ran", &[&fixture.mark("ran")])]),
        );
        input.git = GitObservationWire::Observe;
        let failure = failed(fixture.result(input).await);
        assert_eq!(
            failure.error,
            WorkErrorWire::GitObservation(GitObservationFailureWire::NotAGitWorkspace)
        );
        assert_eq!(
            outcomes(&failure.verification.expect("the command ran")),
            [("ran", VerificationOutcomeWire::Passed)]
        );
        assert_eq!(count(&fixture.marker("ran")), 1);
        assert_eq!(fixture.counts(), before);
    });
}

// ------------------------------------------------------ no verification

fn a_result_without_verification_starts_no_run() {
    block_on(async {
        let mut fixture = Fixture::new("none", true).await;
        let work_item = fixture.started().await;
        let mut partial = result_input(work_item, WorkOutcomeWire::Partial, None);
        partial.verification_summary = Some("given".to_owned());
        partial.git = GitObservationWire::Observe;
        let partial = recorded(fixture.result(partial).await);
        assert_eq!(partial.verification, None);
        assert_eq!(
            partial.result.verification_summary.as_deref(),
            Some("given")
        );
        let done = recorded(
            fixture
                .result(result_input(work_item, WorkOutcomeWire::Complete, None))
                .await,
        );
        assert_eq!(done.verification, None);
        assert_eq!(done.result.verification_summary, None);
        assert_eq!(fixture.runtime.verification_runs(), 0);
    });
}

// ------------------------------------------------------------------ CLI

fn cli_bin() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_BIN_EXE_brainprintd"));
    path.set_file_name(if cfg!(windows) {
        "brainprint.exe"
    } else {
        "brainprint"
    });
    assert!(
        path.is_file(),
        "build the workspace first: {}",
        path.display()
    );
    path
}

struct DaemonGuard(Child);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn cli(home: &Path, args: &[&str], stdin: Option<&str>) -> Child {
    let mut child = Command::new(cli_bin())
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("XDG_RUNTIME_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("brainprint should run");
    let mut input = child.stdin.take().expect("stdin");
    if let Some(text) = stdin {
        input.write_all(text.as_bytes()).expect("write stdin");
    }
    child
}

fn run_cli(home: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    cli(home, args, stdin).wait_with_output().expect("output")
}

fn response_of(output: &Output) -> WorkResponse {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "WorkResponse JSON ({error}): {} / {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn the_cli_runs_verification_and_maps_exit_codes() {
    let home = TestDir::create("cli-home");
    let root = TestDir::create("cli-root");
    workspace_tree(root.path(), false);
    let log = home.path().join("daemon.log");
    let daemon = DaemonGuard(
        Command::new(env!("CARGO_BIN_EXE_brainprintd"))
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env_remove("XDG_RUNTIME_DIR")
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log).expect("log"))
            .spawn()
            .expect("brainprintd"),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !run_cli(home.path(), &["status"], None).status.success() {
        assert!(Instant::now() < deadline, "daemon never became ready");
        thread::sleep(Duration::from_millis(50));
    }
    let root_arg = root.path().to_string_lossy().into_owned();
    assert!(
        run_cli(home.path(), &["init", &root_arg], None)
            .status
            .success()
    );
    let start = serde_json::json!({
        "work_item": {"New": {"source_kind": "Issue", "source_ref": "#52", "title": null, "goal": "cli"}},
        "head": null,
        "git": "Unknown",
        "owner_agent": null
    })
    .to_string();
    let output = run_cli(
        home.path(),
        &["work", "start", "--workspace", &root_arg, "--json"],
        Some(&start),
    );
    let WorkResponse::Started(started) = response_of(&output) else {
        panic!("expected Started")
    };
    let work_item = started.working_state.work_item;
    let result_args = |json: bool| {
        let mut args = vec!["work", "result", "--workspace", root_arg.as_str()];
        if json {
            args.push("--json");
        }
        args
    };

    // A failed command, recorded: exit 0; nothing of argv/env is echoed.
    let mut secret = command("test", &["note:SECRET_ARG_VALUE", "exit:3"]);
    secret.env = vec![(
        "BRAINPRINT_SECRET".to_owned(),
        "SECRET_ENV_VALUE".to_owned(),
    )];
    let failing = serde_json::to_string(&result_input(
        work_item,
        WorkOutcomeWire::Partial,
        verification(vec![secret]),
    ))
    .expect("json");
    let output = run_cli(home.path(), &result_args(true), Some(&failing));
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("SECRET"));
    let report = recorded(response_of(&output));
    assert_eq!(
        outcomes(&report.verification.expect("ran")),
        [("test", VerificationOutcomeWire::Failed { exit_code: 3 })]
    );
    let output = run_cli(home.path(), &result_args(false), Some(&failing));
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let compact = String::from_utf8_lossy(&output.stdout);
    assert!(
        compact.contains("verification test: Failed { exit_code: 3 } ")
            && compact.contains(" stdout 0B stderr 0B"),
        "{compact}"
    );
    assert!(
        !compact.contains("SECRET") && !compact.contains(HELPER),
        "{compact}"
    );

    // A malformed request: exit 2.
    let mut both = result_input(
        work_item,
        WorkOutcomeWire::Partial,
        verification(vec![command("x", &["exit:0"])]),
    );
    both.verification_summary = Some("given".to_owned());
    let output = run_cli(
        home.path(),
        &result_args(true),
        Some(&serde_json::to_string(&both).expect("json")),
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");

    // Busy: exit 4, while the first still records with exit 0.
    let started_marker = home.path().join("started");
    let slow = serde_json::to_string(&result_input(
        work_item,
        WorkOutcomeWire::Partial,
        verification(vec![command(
            "slow",
            &[&format!("mark:{}", started_marker.display()), "sleep:2000"],
        )]),
    ))
    .expect("json");
    let running = cli(home.path(), &result_args(true), Some(&slow));
    wait_for(&started_marker);
    let output = run_cli(
        home.path(),
        &result_args(true),
        Some(
            &serde_json::to_string(&result_input(
                work_item,
                WorkOutcomeWire::Partial,
                verification(vec![command("again", &["exit:0"])]),
            ))
            .expect("json"),
        ),
    );
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let failure = failed(response_of(&output));
    assert_eq!(failure.error, WorkErrorWire::VerificationBusy);
    assert_eq!(failure.verification, None);
    let first = running.wait_with_output().expect("first");
    assert_eq!(first.status.code(), Some(0), "{first:?}");
    assert!(recorded(response_of(&first)).verification.is_some());

    drop(daemon);
    let log = fs::read_to_string(&log).expect("daemon log");
    assert!(log.contains("verification test"), "{log}");
    assert!(!log.contains("SECRET") && !log.contains(HELPER), "{log}");
}

// ------------------------------------------------------------- protocol

fn protocol_version_is_11() {
    assert_eq!(PROTOCOL_VERSION, 11);
}

/// v10 client → v11 daemon: refused at handshake; a verification request
/// sent anyway is never served and runs nothing.
fn a_v10_client_is_refused_by_a_v11_daemon() {
    block_on(async {
        let fixture = Fixture::new("v10-client", false).await;
        let mut connection = connect(&fixture.endpoint).await;
        assert_eq!(
            handshake(&mut connection, 10).await,
            Response::Handshake(HandshakeResponse::VersionMismatch {
                server_protocol_version: 11,
                client_protocol_version: 10,
            })
        );
        let request = work_request(
            &fixture.paths,
            WorkOperationWire::Result(result_input(
                WorkItemId::generate(),
                WorkOutcomeWire::Partial,
                verification(vec![command("never", &[&fixture.mark("never")])]),
            )),
        );
        let _ = protocol::framing::write_message(&mut connection, &request).await;
        let reply: std::io::Result<Response> =
            protocol::framing::read_message(&mut connection).await;
        assert!(reply.is_err(), "a mismatched client must not be served");
        assert!(!fixture.marker("never").exists());
        assert_eq!(fixture.runtime.verification_runs(), 0);
    });
}

/// v11 client → v10 daemon: the client reports the mismatch and stops.
fn a_v11_client_stops_at_a_v10_daemon() {
    block_on(async {
        let home = TestDir::create("v10-daemon");
        let endpoint = runtime_paths::resolve(&GlobalPaths::from_home(home.path()));
        #[cfg(unix)]
        let listener = {
            fs::create_dir_all(endpoint.socket_path.parent().expect("parent")).expect("dir");
            protocol::Listener::bind(&endpoint.socket_path).expect("listener")
        };
        #[cfg(windows)]
        let listener = protocol::Listener::bind(&endpoint.pipe_name).expect("listener");
        let fake_v10 = tokio::spawn(async move {
            let mut listener = listener;
            let mut connection = listener.accept().await.expect("accept");
            let request: Request = protocol::framing::read_message(&mut connection)
                .await
                .expect("handshake");
            let Request::Handshake(handshake) = request else {
                panic!("the first message must be the handshake")
            };
            protocol::framing::write_message(
                &mut connection,
                &Response::Handshake(HandshakeResponse::VersionMismatch {
                    server_protocol_version: 10,
                    client_protocol_version: handshake.protocol_version,
                }),
            )
            .await
            .expect("reply");
            handshake.protocol_version
        });
        let mut connection = brainprint_daemon::client::connect(&endpoint)
            .await
            .expect("connect");
        let error = brainprint_daemon::client::handshake(&mut connection, "i6-task3-test")
            .await
            .expect_err("a v10 daemon must be refused");
        assert!(
            matches!(
                error,
                brainprint_daemon::client::ClientError::VersionMismatch {
                    server_protocol_version: 10,
                    client_protocol_version: 11,
                }
            ),
            "{error:?}"
        );
        assert_eq!(fake_v10.await.expect("fake daemon"), 11);
    });
}

// ------------------------------------------------------------------ MCP

/// The raw `tools/list` reply of the real MCP server, byte for byte as at
/// the #52 base: the verification wire reaches no MCP tool.
fn mcp_tools_list_is_byte_identical() {
    use rmcp::ServiceExt as _;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    let reply = block_on(async {
        let (client, server) = tokio::io::duplex(256 * 1024);
        let _server = tokio::spawn(async move {
            let running = brainprint_mcp::BrainprintMcp::new(env::temp_dir())
                .serve(server)
                .await
                .expect("server should start");
            let _ = running.waiting().await;
        });
        let (read, mut write) = tokio::io::split(client);
        let mut lines = BufReader::new(read).lines();
        for message in [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"i6-task3","version":"0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        ] {
            write.write_all(message.as_bytes()).await.expect("write");
            write.write_all(b"\n").await.expect("write");
        }
        loop {
            let line = lines.next_line().await.expect("read").expect("a reply");
            if line.starts_with(r#"{"jsonrpc":"2.0","id":2,"#) {
                break line;
            }
        }
    });
    let expected = include_str!("fixtures/i6_task3_mcp_tools_list.json");
    assert_eq!(reply, expected.trim_end());
}

// --------------------------------------------------------------- static

/// Commands come only from the request: the verification path reads no
/// repository file, wraps no shell, and MCP names no verification.
fn verification_finds_no_command_and_mcp_exposes_none() {
    for (name, source) in [
        (
            "engine verification.rs",
            include_str!("../../engine/src/verification.rs"),
        ),
        ("daemon verify.rs", include_str!("../src/query/verify.rs")),
        ("daemon handler.rs", include_str!("../src/query/handler.rs")),
    ] {
        let code = source.split("#[cfg(test)]").next().expect("code");
        for forbidden in [
            "package.json",
            "Makefile",
            "Cargo.toml",
            "CLAUDE.md",
            "AGENTS.md",
            "read_to_string",
            "fs::read(",
            "read_dir",
            "\"sh\"",
            "\"cmd\"",
            "unsafe",
        ] {
            assert!(
                !code.contains(forbidden),
                "{name} must not reference {forbidden}"
            );
        }
    }
    let mcp = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mcp/src");
    let mut pending = vec![mcp];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("mcp src") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let text = fs::read_to_string(&path).expect("mcp source");
                assert!(
                    !text.to_lowercase().contains("verification"),
                    "{} names verification",
                    path.display()
                );
            }
        }
    }
}
