//! #53 step 3 acceptance: a verification command's explicit `capture`
//! (compact diagnostics, ephemeral raw artifacts) and `ArtifactRead`,
//! through the real daemon protocol and the real CLI.
//!
//! `harness = false`: this test binary is also the portable helper the
//! commands run (no shell). Helper ops, in order: `exit:<code>`,
//! `sleep:<ms>`, `mark:<path>` (create), `out:<n>` / `err:<n>` (n bytes of
//! a non-periodic pattern on stdout / stderr), `cat:<path>` (a file's
//! bytes to stdout).

use std::{
    env, fs,
    future::Future,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    process::{self, Child, Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use brainprint_core::{
    PROTOCOL_VERSION, WorkItemId,
    protocol::{
        self, ClientConnection, HandshakeRequest, HandshakeResponse, InitRequest, InitResponse,
        Request, Response,
        artifact::{
            ArtifactPartWire, ArtifactReadErrorWire, ArtifactReadRequestWire,
            ArtifactReadResponseWire, MAX_ARTIFACT_READ_BYTES,
        },
        framing::MAX_MESSAGE_BYTES,
        query::{WorkItemSourceKindWire, WorkspaceSelectorWire},
        work::*,
    },
};
use brainprint_daemon::{
    artifacts::{Accounting, MAX_COMMAND_BYTES},
    query::DaemonQueryRuntime,
    runtime_paths,
    server::Server,
};
use brainprint_engine::{
    git_observation::{GitEntryStatus, GitObservation},
    git_status,
    output_capture::HeadTail,
    paths::{GlobalPaths, WorkspacePaths},
};

const HELPER: &str = "--brainprint-capture-helper";
const MIB: u64 = 1024 * 1024;
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
        ("no_capture_is_the_52_run", no_capture_is_the_52_run),
        (
            "raw_capture_reads_back_head_and_tail_exactly",
            raw_capture_reads_back_head_and_tail_exactly,
        ),
        (
            "diagnostics_only_keep_no_artifact",
            diagnostics_only_keep_no_artifact,
        ),
        ("raw_and_diagnostics_together", raw_and_diagnostics_together),
        (
            "an_empty_capture_runs_nothing",
            an_empty_capture_runs_nothing,
        ),
        (
            "unstarted_and_skipped_commands_are_not_run",
            unstarted_and_skipped_commands_are_not_run,
        ),
        (
            "a_timed_out_command_keeps_a_partial_capture",
            a_timed_out_command_keeps_a_partial_capture,
        ),
        (
            "a_full_store_still_runs_and_records",
            a_full_store_still_runs_and_records,
        ),
        (
            "each_capture_is_stored_before_the_next_command",
            each_capture_is_stored_before_the_next_command,
        ),
        (
            "a_disconnect_removes_the_requests_artifacts",
            a_disconnect_removes_the_requests_artifacts,
        ),
        (
            "same_batch_eviction_is_reported_unavailable",
            same_batch_eviction_is_reported_unavailable,
        ),
        (
            "diagnostics_fit_the_response_budget",
            diagnostics_fit_the_response_budget,
        ),
        (
            "a_restart_expires_old_handles",
            a_restart_expires_old_handles,
        ),
        (
            "observe_sees_command_files_not_artifacts",
            observe_sees_command_files_not_artifacts,
        ),
        (
            "the_cli_writes_raw_bytes_exactly",
            the_cli_writes_raw_bytes_exactly,
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

/// Byte `i` of a generated stream: never periodic at a power of two, so a
/// wrong offset shows.
fn pattern(i: u64) -> u8 {
    (i % 251) as u8
}

fn patterned(total: u64, mut stream: impl std::io::Write) {
    let mut chunk = vec![0_u8; 64 * 1024];
    let mut at = 0;
    while at < total {
        let size = chunk.len().min((total - at) as usize);
        for (offset, byte) in chunk[..size].iter_mut().enumerate() {
            *byte = pattern(at + offset as u64);
        }
        stream.write_all(&chunk[..size]).expect("write");
        at += size as u64;
    }
    stream.flush().expect("flush");
}

fn helper(ops: &[String]) {
    for op in ops {
        let (name, value) = op.split_once(':').expect("op:value");
        match name {
            "exit" => process::exit(value.parse().expect("code")),
            "sleep" => thread::sleep(Duration::from_millis(value.parse().expect("ms"))),
            "mark" => fs::write(value, b"x").expect("marker"),
            "out" => patterned(value.parse().expect("bytes"), std::io::stdout().lock()),
            "err" => patterned(value.parse().expect("bytes"), std::io::stderr().lock()),
            "cat" => {
                let mut stdout = std::io::stdout().lock();
                stdout
                    .write_all(&fs::read(value).expect("cat file"))
                    .expect("write");
                stdout.flush().expect("flush");
            }
            other => panic!("unknown helper op {other}"),
        }
    }
}

fn helper_argv(ops: &[&str]) -> Vec<String> {
    let exe = env::current_exe().expect("exe");
    [exe.to_string_lossy().as_ref(), HELPER]
        .into_iter()
        .chain(ops.iter().copied())
        .map(str::to_owned)
        .collect()
}

fn command(
    label: &str,
    ops: &[&str],
    capture: Option<VerificationCaptureWire>,
) -> VerificationCommandWire {
    VerificationCommandWire {
        label: label.to_owned(),
        argv: helper_argv(ops),
        cwd: None,
        env: Vec::new(),
        timeout_secs: 60,
        capture,
    }
}

const RAW: Option<VerificationCaptureWire> = Some(VerificationCaptureWire {
    raw: true,
    diagnostics: None,
});

const fn diagnostics(raw: bool, format: DiagnosticFormatWire) -> Option<VerificationCaptureWire> {
    Some(VerificationCaptureWire {
        raw,
        diagnostics: Some(format),
    })
}

fn result_input(
    work_item: WorkItemId,
    commands: Vec<VerificationCommandWire>,
) -> WorkResultInputWire {
    WorkResultInputWire {
        work_item,
        outcome: WorkOutcomeWire::Partial,
        summary: "done".to_owned(),
        commit_id: None,
        verification_summary: None,
        verification: Some(VerificationWire { commands }),
        verification_job: None,
        git: GitObservationWire::Unknown,
        change_set: None,
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "{what} never happened");
        thread::sleep(Duration::from_millis(20));
    }
}

// --------------------------------------------------------------- harness

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "brainprint-i6-task4c-{label}-{}-{sequence}",
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
    assert!(output.status.success(), "git {args:?}");
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

async fn open_connection(endpoint: &runtime_paths::RuntimeEndpoint) -> ClientConnection {
    #[cfg(unix)]
    let connection = ClientConnection::connect(&endpoint.socket_path).await;
    #[cfg(windows)]
    let connection = ClientConnection::connect(&endpoint.pipe_name).await;
    let mut connection = connection.expect("connect should succeed");
    let reply = send(
        &mut connection,
        &Request::Handshake(HandshakeRequest {
            protocol_version: PROTOCOL_VERSION,
            client_kind: "i6-task4-test".to_owned(),
        }),
    )
    .await;
    assert!(matches!(
        reply,
        Response::Handshake(HandshakeResponse::Ok { .. })
    ));
    connection
}

fn work_request(paths: &WorkspacePaths, operation: WorkOperationWire) -> Request {
    Request::Work(WorkRequest {
        workspace: WorkspaceSelectorWire::Locator {
            path: paths.workspace_root.to_string_lossy().into_owned(),
        },
        operation,
    })
}

/// Row counts of every work table: the "nothing written" witness.
fn counts(paths: &WorkspacePaths) -> Vec<i64> {
    let connection = rusqlite::Connection::open_with_flags(
        &paths.workspace_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("workspace.db");
    ["work_item", "working_state", "work_result"]
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

/// An in-process daemon with one initialized Workspace.
struct Fixture {
    home: TestDir,
    _root: TestDir,
    endpoint: runtime_paths::RuntimeEndpoint,
    connection: ClientConnection,
    paths: WorkspacePaths,
    runtime: Arc<DaemonQueryRuntime>,
    artifact_dir: PathBuf,
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
        let response = send(
            &mut connection,
            &Request::Init(InitRequest {
                path: root.path().to_string_lossy().into_owned(),
            }),
        )
        .await;
        let Response::Init(InitResponse { workspace_root, .. }) = response else {
            panic!("expected Init, got {response:?}")
        };
        Self {
            artifact_dir: global_paths
                .runtime_root()
                .join("artifacts")
                .join("verification"),
            home,
            _root: root,
            endpoint,
            connection,
            paths: WorkspacePaths::from_root(PathBuf::from(workspace_root)),
            runtime,
            _server: server_task,
        }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    fn mark(&self, name: &str) -> String {
        format!("mark:{}", self.file(name).display())
    }

    async fn work(&mut self, operation: WorkOperationWire) -> WorkResponse {
        match send(&mut self.connection, &work_request(&self.paths, operation)).await {
            Response::Work(response) => response,
            other => panic!("expected Work, got {other:?}"),
        }
    }

    async fn started(&mut self) -> WorkItemId {
        let operation = WorkOperationWire::Start(WorkStartWire {
            work_item: WorkStartItemWire::New(NewWorkItemWire {
                source_kind: WorkItemSourceKindWire::Issue,
                source_ref: Some("#53".to_owned()),
                title: None,
                goal: "capture".to_owned(),
            }),
            head: None,
            git: GitObservationWire::Unknown,
            owner_agent: None,
        });
        match self.work(operation).await {
            WorkResponse::Started(started) => started.working_state.work_item,
            other => panic!("expected Started, got {other:?}"),
        }
    }

    /// Start a WorkItem and record a Result running `commands`.
    async fn verify(&mut self, commands: Vec<VerificationCommandWire>) -> WorkResponse {
        let work_item = self.started().await;
        self.work(WorkOperationWire::Result(result_input(work_item, commands)))
            .await
    }

    async fn read(
        &mut self,
        handle: &str,
        stream: OutputStreamWire,
        part: ArtifactPartWire,
        offset: u32,
        max_bytes: u32,
    ) -> ArtifactReadResponseWire {
        let request = Request::ArtifactRead(ArtifactReadRequestWire {
            handle: handle.to_owned(),
            stream,
            part,
            offset,
            max_bytes,
        });
        match send(&mut self.connection, &request).await {
            Response::ArtifactRead(response) => response,
            other => panic!("expected ArtifactRead, got {other:?}"),
        }
    }

    /// A whole part by continuation, at most 512 KiB per read; each reply
    /// frame is checked against the 1 MiB limit.
    async fn read_part(
        &mut self,
        handle: &str,
        stream: OutputStreamWire,
        part: ArtifactPartWire,
    ) -> (Vec<u8>, u64) {
        let mut bytes = Vec::new();
        let mut offset = 0;
        loop {
            let response = self
                .read(handle, stream, part, offset, MAX_ARTIFACT_READ_BYTES)
                .await;
            let frame = serde_json::to_vec(&Response::ArtifactRead(response.clone()))
                .expect("json")
                .len();
            assert!(frame < MAX_MESSAGE_BYTES as usize, "{frame}");
            let (start, next) = data(&response, &mut bytes);
            match next {
                Some(next) => offset = next,
                None => return (bytes, start),
            }
        }
    }

    fn accounting(&self) -> Accounting {
        self.runtime.artifacts().accounting()
    }

    fn payload_files(&self) -> usize {
        fs::read_dir(&self.artifact_dir).map_or(0, Iterator::count)
    }
}

/// Appends a `Data` reply's bytes; its original start and next offset.
fn data(response: &ArtifactReadResponseWire, into: &mut Vec<u8>) -> (u64, Option<u32>) {
    let ArtifactReadResponseWire::Data {
        original_start_offset,
        data_b64,
        returned_bytes,
        next_offset,
        ..
    } = response
    else {
        panic!("expected Data, got {response:?}")
    };
    let bytes = STANDARD.decode(data_b64).expect("standard base64");
    assert_eq!(bytes.len(), *returned_bytes as usize);
    into.extend_from_slice(&bytes);
    (*original_start_offset, *next_offset)
}

fn results_of(response: WorkResponse) -> Vec<CommandResultWire> {
    match response {
        WorkResponse::Recorded(recorded) => recorded.verification.expect("verification ran"),
        other => panic!("expected Recorded, got {other:?}"),
    }
}

fn captured(
    result: &CommandResultWire,
) -> (
    StreamStatusWire,
    Option<&DiagnosticSummaryWire>,
    &RawAvailabilityWire,
) {
    match &result.capture {
        CommandCaptureWire::Captured {
            stream_status,
            diagnostics,
            raw,
        } => (*stream_status, diagnostics.as_ref(), raw),
        other => panic!("{}: expected Captured, got {other:?}", result.label),
    }
}

fn available(raw: &RawAvailabilityWire) -> &RawArtifactRefWire {
    match raw {
        RawAvailabilityWire::Available(reference) => reference,
        other => panic!("expected Available, got {other:?}"),
    }
}

fn unavailable() -> ArtifactReadResponseWire {
    ArtifactReadResponseWire::Failed {
        error: ArtifactReadErrorWire::ArtifactUnavailable,
    }
}

fn patterned_bytes(range: std::ops::Range<u64>) -> Vec<u8> {
    range.map(pattern).collect()
}

const OUT: OutputStreamWire = OutputStreamWire::Stdout;
const ERR: OutputStreamWire = OutputStreamWire::Stderr;
const HEAD: ArtifactPartWire = ArtifactPartWire::Head;
const TAIL: ArtifactPartWire = ArtifactPartWire::Tail;

// ----------------------------------------------------------------- tests

/// `capture: None` is #52 exactly: counts only, no reservation, no file.
fn no_capture_is_the_52_run() {
    block_on(async {
        let mut fixture = Fixture::new("none", false).await;
        let results = results_of(
            fixture
                .verify(vec![command("plain", &["out:12345", "err:678"], None)])
                .await,
        );
        assert_eq!(results[0].outcome, VerificationOutcomeWire::Passed);
        assert_eq!(
            (results[0].stdout_bytes, results[0].stderr_bytes),
            (12345, 678)
        );
        assert_eq!(results[0].capture, CommandCaptureWire::NotRequested);
        assert_eq!(fixture.accounting(), Accounting::default());
        assert_eq!(fixture.payload_files(), 0);
    });
}

fn raw_capture_reads_back_head_and_tail_exactly() {
    block_on(async {
        let mut fixture = Fixture::new("raw", false).await;
        let total = 9 * MIB + 123;
        let results = results_of(
            fixture
                .verify(vec![command(
                    "raw",
                    &[&format!("out:{total}"), "err:1000"],
                    RAW,
                )])
                .await,
        );
        assert_eq!(results[0].stdout_bytes, total);
        let (status, diagnostics, raw) = captured(&results[0]);
        assert_eq!(status, StreamStatusWire::Complete);
        assert!(diagnostics.is_none());
        let reference = available(raw).clone();
        assert_eq!(
            reference.stdout,
            RawStreamMetaWire {
                observed_bytes: total,
                head_bytes: 4 * MIB,
                tail_bytes: 4 * MIB,
                omitted_bytes: total - 8 * MIB,
                truncated: true,
                tail_start_offset: total - 4 * MIB,
            }
        );
        assert_eq!(
            reference.stderr,
            RawStreamMetaWire {
                observed_bytes: 1000,
                head_bytes: 1000,
                tail_bytes: 0,
                omitted_bytes: 0,
                truncated: false,
                tail_start_offset: 1000,
            }
        );
        let handle = reference.handle.as_str();
        assert!(!handle.contains('/') && !handle.contains('\\'));

        // Continuation restores each retained part; the middle is never
        // pretended to exist.
        let (head, start) = fixture.read_part(handle, OUT, HEAD).await;
        assert_eq!(start, 0);
        assert!(head == patterned_bytes(0..4 * MIB), "stdout head differs");
        let (tail, start) = fixture.read_part(handle, OUT, TAIL).await;
        assert_eq!(start, total - 4 * MIB);
        assert!(
            tail == patterned_bytes(total - 4 * MIB..total),
            "tail differs"
        );
        let (head, _) = fixture.read_part(handle, ERR, HEAD).await;
        assert_eq!(head, patterned_bytes(0..1000));
        assert_eq!(
            fixture.read(handle, ERR, TAIL, 0, 10).await,
            ArtifactReadResponseWire::Data {
                stream: ERR,
                part: TAIL,
                original_start_offset: 1000,
                data_b64: String::new(),
                returned_bytes: 0,
                next_offset: None,
            }
        );

        // Offsets are within the part; `original_start_offset` is the
        // part's start in the stream, the same for every chunk.
        let mut one = Vec::new();
        let (start, next) = data(&fixture.read(handle, OUT, TAIL, 1000, 1).await, &mut one);
        assert_eq!((start, next), (total - 4 * MIB, Some(1001)));
        assert_eq!(one, [pattern(total - 4 * MIB + 1000)]);
        let mut end = Vec::new();
        let near_end = (4 * MIB - 10) as u32;
        let (_, next) = data(
            &fixture
                .read(handle, OUT, HEAD, near_end, MAX_ARTIFACT_READ_BYTES)
                .await,
            &mut end,
        );
        assert_eq!((end.len(), next), (10, None));
        let mut past = Vec::new();
        let (_, next) = data(
            &fixture
                .read(handle, OUT, HEAD, (4 * MIB + 5) as u32, 16)
                .await,
            &mut past,
        );
        assert_eq!((past.len(), next), (0, None));

        for max_bytes in [0, MAX_ARTIFACT_READ_BYTES + 1] {
            assert!(matches!(
                fixture.read(handle, OUT, HEAD, 0, max_bytes).await,
                ArtifactReadResponseWire::Failed {
                    error: ArtifactReadErrorWire::InvalidRequest { .. }
                }
            ));
        }
        for unknown in [
            uuid::Uuid::new_v4().to_string(),
            "../../etc/passwd".to_owned(),
            fixture.artifact_dir.join("0.payload").display().to_string(),
        ] {
            assert_eq!(
                fixture.read(&unknown, OUT, HEAD, 0, 16).await,
                unavailable()
            );
        }
    });
}

/// `src/app.rs` is a Workspace file, `external.rs` sits outside it,
/// `missing.rs` does not exist.
fn path_line_column_output(fixture: &Fixture) -> PathBuf {
    let external = fixture.file("external.rs");
    fs::write(&external, "// outside\n").expect("external");
    let mut text = format!(
        "src/app.rs:1:5: error: in the workspace\n\
         {}:2:3: warning: outside it\n\
         missing.rs:3:4: note: nowhere\n\
         src/app.rs:1:5: error: in the workspace\n\
         this line is not a diagnostic\n\
         src/app.rs:9:9: error: {}\n",
        external.display(),
        "é".repeat(1500),
    );
    for line in 0..64 {
        text.push_str(&format!("src/app.rs:{}:1: help: h{line}\n", line + 10));
    }
    let output = fixture.file("plc.txt");
    fs::write(&output, text).expect("fixture");
    output
}

fn cargo_message(level: &str, code: Option<&str>, message: &str, spans: &str) -> String {
    let code = code.map_or("null".to_owned(), |code| {
        format!(r#"{{"code":"{code}","explanation":null}}"#)
    });
    format!(
        r#"{{"reason":"compiler-message","package_id":"x 0.1.0","manifest_path":"/w/Cargo.toml","target":{{"kind":["lib"],"name":"x","src_path":"/w/src/lib.rs","edition":"2021","doc":true,"doctest":true,"test":true}},"message":{{"$message_type":"diagnostic","children":[],"code":{code},"level":"{level}","message":"{message}","spans":[{spans}],"rendered":"rendered text"}}}}"#
    )
}

fn cargo_span(file: &str, line: u32) -> String {
    format!(
        r#"{{"byte_end":10,"byte_start":5,"column_end":9,"column_start":7,"expansion":null,"file_name":"{file}","is_primary":true,"label":null,"line_end":{line},"line_start":{line},"suggested_replacement":null,"suggestion_applicability":null,"text":[]}}"#
    )
}

fn cargo_output(fixture: &Fixture) -> PathBuf {
    let text = [
        cargo_message(
            "error",
            Some("E0308"),
            "mismatched types",
            &cargo_span("src/app.rs", 1),
        ),
        cargo_message("warning", None, "2 warnings emitted", ""),
        // A direct `rustc --error-format=json` line: not supported.
        r#"{"$message_type":"diagnostic","children":[],"code":null,"level":"error","message":"direct rustc","spans":[],"rendered":"error"}"#.to_owned(),
    ]
    .join("\n");
    let output = fixture.file("cargo.json");
    fs::write(&output, text + "\n").expect("fixture");
    output
}

fn diagnostics_only_keep_no_artifact() {
    block_on(async {
        let mut fixture = Fixture::new("diagnostics", false).await;
        let plc = path_line_column_output(&fixture);
        let cargo = cargo_output(&fixture);
        let results = results_of(
            fixture
                .verify(vec![
                    command(
                        "plc",
                        &[&format!("cat:{}", plc.display())],
                        diagnostics(false, DiagnosticFormatWire::PathLineColumn),
                    ),
                    command(
                        "cargo",
                        &[&format!("cat:{}", cargo.display())],
                        diagnostics(false, DiagnosticFormatWire::CargoCompilerMessageJson),
                    ),
                ])
                .await,
        );

        let (status, summary, raw) = captured(&results[0]);
        assert_eq!(status, StreamStatusWire::Complete);
        assert_eq!(raw, &RawAvailabilityWire::NotRequested);
        let summary = summary.expect("diagnostics");
        // 69 parsed: one exact duplicate, 64 kept, 4 past the bound.
        assert_eq!(
            (
                summary.items.len(),
                summary.observed,
                summary.deduplicated,
                summary.omitted,
                summary.parse_misses,
                summary.delivery_omitted
            ),
            (64, 69, 1, 4, 1, 0)
        );
        let workspace = DiagnosticPathWire::Workspace {
            path: "src/app.rs".to_owned(),
        };
        let first = &summary.items[0];
        assert_eq!(
            (
                first.severity,
                first.message.as_str(),
                &first.path,
                first.line,
                first.column,
                first.stream
            ),
            (
                DiagnosticSeverityWire::Error,
                "in the workspace",
                &workspace,
                Some(1),
                Some(5),
                OUT
            )
        );
        let long = &summary.items[1];
        assert!(long.message_truncated);
        assert!(long.message.len() <= 2048 && long.message.chars().all(|c| c == 'é'));
        let by_message = |text: &str| {
            summary
                .items
                .iter()
                .find(|item| item.message == text)
                .unwrap_or_else(|| panic!("{text}"))
        };
        let external = by_message("outside it");
        assert_eq!(external.path, DiagnosticPathWire::External);
        assert_eq!(external.severity, DiagnosticSeverityWire::Warning);
        let nowhere = by_message("nowhere");
        assert_eq!(nowhere.path, DiagnosticPathWire::Unresolved);
        assert_eq!(nowhere.severity, DiagnosticSeverityWire::Note);

        let (_, summary, raw) = captured(&results[1]);
        assert_eq!(raw, &RawAvailabilityWire::NotRequested);
        let summary = summary.expect("diagnostics");
        assert_eq!(
            (summary.items.len(), summary.observed, summary.parse_misses),
            (2, 2, 1)
        );
        assert_eq!(summary.items[0].code.as_deref(), Some("E0308"));
        assert_eq!(summary.items[0].path, workspace);
        assert_eq!(summary.items[0].line, Some(1));
        assert_eq!(summary.items[1].path, DiagnosticPathWire::Absent);
        assert_eq!(summary.items[1].severity, DiagnosticSeverityWire::Warning);

        // No raw request: no reservation, no artifact on disk.
        assert_eq!(fixture.accounting(), Accounting::default());
        assert_eq!(fixture.payload_files(), 0);
    });
}

fn raw_and_diagnostics_together() {
    block_on(async {
        let mut fixture = Fixture::new("both", false).await;
        let cargo = cargo_output(&fixture);
        let results = results_of(
            fixture
                .verify(vec![command(
                    "both",
                    &[&format!("cat:{}", cargo.display()), "exit:101"],
                    diagnostics(true, DiagnosticFormatWire::CargoCompilerMessageJson),
                )])
                .await,
        );
        assert_eq!(
            results[0].outcome,
            VerificationOutcomeWire::Failed { exit_code: 101 }
        );
        let (status, summary, raw) = captured(&results[0]);
        assert_eq!(status, StreamStatusWire::Complete);
        assert_eq!(summary.expect("diagnostics").items.len(), 2);
        let handle = available(raw).handle.clone();
        let (head, _) = fixture.read_part(&handle, OUT, HEAD).await;
        assert_eq!(head, fs::read(&cargo).expect("fixture"));
        assert_eq!(fixture.accounting().completed_count, 1);
        assert_eq!(fixture.accounting().reserved_count, 0);
    });
}

fn an_empty_capture_runs_nothing() {
    block_on(async {
        let mut fixture = Fixture::new("empty", false).await;
        let work_item = fixture.started().await;
        let before = counts(&fixture.paths);
        let response = fixture
            .work(WorkOperationWire::Result(result_input(
                work_item,
                vec![
                    command("first", &[&fixture.mark("ran")], RAW),
                    command(
                        "empty",
                        &[],
                        Some(VerificationCaptureWire {
                            raw: false,
                            diagnostics: None,
                        }),
                    ),
                ],
            )))
            .await;
        let WorkResponse::Failed(failure) = response else {
            panic!("expected Failed, got {response:?}")
        };
        assert!(
            matches!(&failure.error, WorkErrorWire::InvalidObservation { reason } if reason.contains("capture requests nothing")),
            "{:?}",
            failure.error
        );
        assert_eq!(failure.verification, None);
        assert!(!fixture.file("ran").exists());
        assert_eq!(fixture.runtime.verification_runs(), 0);
        assert_eq!(counts(&fixture.paths), before);
        assert_eq!(fixture.accounting(), Accounting::default());
        assert_eq!(fixture.payload_files(), 0);
    });
}

fn unstarted_and_skipped_commands_are_not_run() {
    block_on(async {
        let mut fixture = Fixture::new("not-run", false).await;
        let mut missing = command("missing", &[], RAW);
        missing.argv = vec![fixture.file("no-such-program").display().to_string()];
        let results = results_of(
            fixture
                .verify(vec![
                    missing,
                    command("skipped", &[&fixture.mark("ran")], RAW),
                ])
                .await,
        );
        assert!(matches!(
            results[0].outcome,
            VerificationOutcomeWire::NotStarted { .. }
        ));
        assert_eq!(results[1].outcome, VerificationOutcomeWire::Skipped);
        assert_eq!(results[0].capture, CommandCaptureWire::NotRun);
        assert_eq!(results[1].capture, CommandCaptureWire::NotRun);
        assert!(!fixture.file("ran").exists());
        assert_eq!(fixture.accounting(), Accounting::default());
        assert_eq!(fixture.payload_files(), 0);
    });
}

fn a_timed_out_command_keeps_a_partial_capture() {
    block_on(async {
        let mut fixture = Fixture::new("timeout", false).await;
        let mut slow = command("slow", &["out:1000", &format!("sleep:{LATE_MS}")], RAW);
        slow.timeout_secs = 1;
        let results = results_of(fixture.verify(vec![slow]).await);
        assert_eq!(results[0].outcome, VerificationOutcomeWire::TimedOut);
        let (status, _, raw) = captured(&results[0]);
        assert_eq!(status, StreamStatusWire::Partial);
        let handle = available(raw).handle.clone();
        let (head, _) = fixture.read_part(&handle, OUT, HEAD).await;
        assert_eq!(head, patterned_bytes(0..1000));
        assert_eq!(fixture.accounting().reserved_count, 0);
    });
}

/// No room for raw: the command still runs, keeps its diagnostics and is
/// recorded; only the raw reference is `Unavailable`.
fn a_full_store_still_runs_and_records() {
    block_on(async {
        let mut fixture = Fixture::new("full", false).await;
        let runtime = Arc::clone(&fixture.runtime);
        let held: Vec<_> = (0..4)
            .map(|_| {
                runtime
                    .artifacts()
                    .reserve(MAX_COMMAND_BYTES)
                    .expect("reserve")
            })
            .collect();
        let plc = path_line_column_output(&fixture);
        let response = fixture
            .verify(vec![command(
                "full",
                &[&format!("cat:{}", plc.display()), &fixture.mark("ran")],
                diagnostics(true, DiagnosticFormatWire::PathLineColumn),
            )])
            .await;
        let WorkResponse::Recorded(recorded) = response else {
            panic!("expected Recorded, got {response:?}")
        };
        let results = recorded.verification.expect("ran");
        assert_eq!(results[0].outcome, VerificationOutcomeWire::Passed);
        let (status, summary, raw) = captured(&results[0]);
        assert_eq!(status, StreamStatusWire::Complete);
        assert_eq!(summary.expect("diagnostics").items.len(), 64);
        assert_eq!(raw, &RawAvailabilityWire::Unavailable);
        assert!(fixture.file("ran").exists());
        let accounting = fixture.accounting();
        assert_eq!(
            (accounting.reserved_count, accounting.completed_count),
            (4, 0)
        );
        drop(held);
        assert_eq!(fixture.accounting(), Accounting::default());
    });
}

/// A command's capture is committed before the next command starts: while
/// the second runs, the first is stored and only one 16 MiB reservation
/// is held.
fn each_capture_is_stored_before_the_next_command() {
    block_on(async {
        let mut fixture = Fixture::new("per-command", false).await;
        let runtime = Arc::clone(&fixture.runtime);
        let started = fixture.file("second-started");
        let observed = thread::spawn(move || {
            wait_until("the second command", || started.exists());
            runtime.artifacts().accounting()
        });
        let results = results_of(
            fixture
                .verify(vec![
                    command("first", &["out:1000"], RAW),
                    command(
                        "second",
                        &[&fixture.mark("second-started"), "sleep:1500"],
                        RAW,
                    ),
                ])
                .await,
        );
        assert_eq!(
            observed.join().expect("observer"),
            Accounting {
                completed_count: 1,
                completed_bytes: 1000,
                reserved_count: 1,
                reserved_bytes: MAX_COMMAND_BYTES,
            }
        );
        for result in &results {
            available(captured(result).2);
        }
        assert_eq!(
            fixture.accounting(),
            Accounting {
                completed_count: 2,
                completed_bytes: 1000,
                reserved_count: 0,
                reserved_bytes: 0,
            }
        );
    });
}

/// The client leaves while the second command runs: the tree ends,
/// nothing is recorded, and the first command's artifact -- which no
/// response will ever name -- is removed.
fn a_disconnect_removes_the_requests_artifacts() {
    block_on(async {
        let mut fixture = Fixture::new("disconnect", false).await;
        let work_item = fixture.started().await;
        let before = counts(&fixture.paths);
        let request = work_request(
            &fixture.paths,
            WorkOperationWire::Result(result_input(
                work_item,
                vec![
                    command("first", &["out:1000"], RAW),
                    command(
                        "slow",
                        &[
                            &fixture.mark("started"),
                            &format!("sleep:{LATE_MS}"),
                            &fixture.mark("late"),
                        ],
                        RAW,
                    ),
                ],
            )),
        );
        let mut leaving = open_connection(&fixture.endpoint).await;
        protocol::framing::write_message(&mut leaving, &request)
            .await
            .expect("write");
        let started = fixture.file("started");
        wait_until("the slow command", || started.exists());
        assert_eq!(fixture.accounting().completed_count, 1);
        assert_eq!(fixture.payload_files(), 1);
        drop(leaving);

        wait_until("the cleanup", || {
            fixture.accounting() == Accounting::default()
        });
        assert_eq!(fixture.payload_files(), 0);
        thread::sleep(Duration::from_millis(LATE_MS + 500));
        assert!(
            !fixture.file("late").exists(),
            "the tree outlived the client"
        );
        assert_eq!(counts(&fixture.paths), before);
    });
}

/// Five 16 MiB artifacts in one batch: the fifth evicts the first, which
/// the response therefore reports as `Unavailable`, not a dead handle.
fn same_batch_eviction_is_reported_unavailable() {
    block_on(async {
        let mut fixture = Fixture::new("eviction", false).await;
        let full = &format!("out:{}", 8 * MIB);
        let full_err = &format!("err:{}", 8 * MIB);
        let commands = (0..5)
            .map(|index| command(&format!("c{index}"), &[full, full_err], RAW))
            .collect();
        let results = results_of(fixture.verify(commands).await);
        assert_eq!(captured(&results[0]).2, &RawAvailabilityWire::Unavailable);
        for result in &results[1..] {
            let reference = available(captured(result).2);
            assert_eq!(reference.stdout.head_bytes, 8 * MIB);
            assert!(fixture.runtime.artifacts().contains(&reference.handle));
        }
        let accounting = fixture.accounting();
        assert_eq!(
            (accounting.completed_count, accounting.completed_bytes),
            (4, 64 * MIB)
        );
        assert_eq!(accounting.reserved_count, 0);
        // A later batch evicts the oldest one left; its handle, already
        // delivered, now reads as unavailable.
        let oldest = available(captured(&results[1]).2).handle.clone();
        let later = results_of(
            fixture
                .verify(vec![command("later", &[full, full_err], RAW)])
                .await,
        );
        available(captured(&later[0]).2);
        assert_eq!(fixture.read(&oldest, OUT, HEAD, 0, 1).await, unavailable());
    });
}

/// 16 commands × 70 maximal diagnostics: the response keeps the highest
/// priority within 512 KiB, the engine counts stay as they were, and the
/// whole frame stays under 1 MiB.
fn diagnostics_fit_the_response_budget() {
    block_on(async {
        let mut fixture = Fixture::new("budget", false).await;
        let levels = ["error", "warning", "note", "help"];
        let long_path = "p".repeat(1900);
        let commands = (0..16)
            .map(|command_index| {
                let lines: Vec<String> = (0..70)
                    .map(|index| {
                        let message = format!("c{command_index}-i{index}-{}", "m".repeat(2100));
                        cargo_message(
                            levels[(command_index + index) % 4],
                            Some(&format!("{index:04}{}", "k".repeat(1996))),
                            &message,
                            &cargo_span(&long_path, 1),
                        )
                    })
                    .collect();
                let output = fixture.file(&format!("budget-{command_index}.json"));
                fs::write(&output, lines.join("\n") + "\n").expect("fixture");
                command(
                    &format!("c{command_index}"),
                    &[&format!("cat:{}", output.display())],
                    diagnostics(false, DiagnosticFormatWire::CargoCompilerMessageJson),
                )
            })
            .collect();
        let response = fixture.verify(commands).await;
        let frame = serde_json::to_vec(&Response::Work(response.clone()))
            .expect("json")
            .len();
        assert!(frame < MAX_MESSAGE_BYTES as usize, "{frame}");

        let results = results_of(response);
        let mut sent = 0;
        let mut delivered = Vec::new();
        for result in &results {
            let summary = captured(result).1.expect("diagnostics");
            assert_eq!(
                (summary.observed, summary.omitted, summary.parse_misses),
                (70, 6, 0),
                "{}",
                result.label
            );
            assert_eq!(summary.items.len() as u64 + summary.delivery_omitted, 64);
            for item in &summary.items {
                assert_eq!(item.severity, DiagnosticSeverityWire::Error);
                assert!(item.message_truncated);
                assert_eq!(item.path, DiagnosticPathWire::Unresolved);
                sent += serde_json::to_vec(item).expect("json").len();
            }
            delivered.push(summary.items.len());
        }
        assert!(sent <= 512 * 1024, "{sent}");
        // Errors first, in command order: whole commands, then one cut
        // short, then nothing.
        // (The engine keeps a command's highest severities: all its errors.)
        let errors = |command: usize| {
            (0..70)
                .filter(|index| (command + index).is_multiple_of(4))
                .count()
        };
        let whole = (0..16).take_while(|&c| delivered[c] == errors(c)).count();
        assert!(whole > 0 && whole < 16, "{delivered:?}");
        assert!(delivered[whole] < errors(whole));
        assert!(
            delivered[whole + 1..].iter().all(|&n| n == 0),
            "{delivered:?}"
        );
        // Within a command, the engine's order.
        let summary = captured(&results[0]).1.expect("diagnostics");
        let indices: Vec<usize> = summary
            .items
            .iter()
            .map(|item| {
                item.message
                    .split('-')
                    .nth(1)
                    .and_then(|index| index[1..].parse().ok())
                    .expect("index")
            })
            .collect();
        assert!(
            indices.windows(2).all(|pair| pair[0] < pair[1]),
            "{indices:?}"
        );
    });
}

fn a_restart_expires_old_handles() {
    block_on(async {
        let home = TestDir::create("restart-home");
        let global_paths = GlobalPaths::from_home(home.path());
        let endpoint = runtime_paths::resolve(&global_paths);
        let mut first = Server::bind(&global_paths).await.expect("bind");
        let mut stream = HeadTail::default();
        stream.push(b"before the restart");
        let handle = first
            .query_runtime()
            .artifacts()
            .insert(&stream.finish(), &HeadTail::default().finish())
            .expect("insert")
            .handle;
        let task = tokio::spawn(async move {
            let _ = first.serve().await;
        });
        let request = Request::ArtifactRead(ArtifactReadRequestWire {
            handle: handle.clone(),
            stream: OUT,
            part: HEAD,
            offset: 0,
            max_bytes: 64,
        });
        let mut connection = open_connection(&endpoint).await;
        let Response::ArtifactRead(read) = send(&mut connection, &request).await else {
            panic!("expected ArtifactRead")
        };
        let mut bytes = Vec::new();
        data(&read, &mut bytes);
        assert_eq!(bytes, b"before the restart");
        drop(connection);
        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut second = Server::bind(&global_paths).await.expect("rebind");
        let task = tokio::spawn(async move {
            let _ = second.serve().await;
        });
        let mut connection = open_connection(&endpoint).await;
        assert_eq!(
            send(&mut connection, &request).await,
            Response::ArtifactRead(unavailable())
        );
        task.abort();
    });
}

/// Observe still runs after the commands and sees a file they wrote in
/// the Workspace; the raw artifact lives outside it and never shows.
fn observe_sees_command_files_not_artifacts() {
    block_on(async {
        let mut fixture = Fixture::new("observe", true).await;
        let root = fixture.paths.workspace_root.clone();
        let work_item = fixture.started().await;
        let generate = format!("mark:{}", root.join("generated.txt").display());
        let mut input = result_input(
            work_item,
            vec![command("gen", &[&generate, "out:5000"], RAW)],
        );
        input.git = GitObservationWire::Observe;
        let results = match fixture.work(WorkOperationWire::Result(input)).await {
            WorkResponse::Recorded(recorded) => recorded.verification.expect("ran"),
            other => panic!("expected Recorded, got {other:?}"),
        };
        available(captured(&results[0]).2);
        assert_eq!(fixture.payload_files(), 1);
        assert!(!fixture.artifact_dir.starts_with(&root));
        let GitObservation::Dirty(entries) =
            git_status::observe(&root).expect("observe").observation
        else {
            panic!("the command left a file")
        };
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].path, "generated.txt");
        assert_eq!(entries[0].status, GitEntryStatus::Untracked);
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

fn run_cli(home: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(cli_bin())
        .env(brainprint_core::lifecycle::NO_AUTOSTART_ENV, "1")
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
    drop(input);
    child.wait_with_output().expect("output")
}

fn the_cli_writes_raw_bytes_exactly() {
    let home = TestDir::create("cli-home");
    let root = TestDir::create("cli-root");
    workspace_tree(root.path(), false);
    let log = home.path().join("daemon.log");
    let _daemon = DaemonGuard(
        Command::new(env!("CARGO_BIN_EXE_brainprintd"))
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env_remove("XDG_RUNTIME_DIR")
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log).expect("log"))
            .spawn()
            .expect("brainprintd"),
    );
    wait_until("the daemon", || {
        run_cli(home.path(), &["status"], None).status.success()
    });
    let root_arg = root.path().to_string_lossy().into_owned();
    assert!(
        run_cli(home.path(), &["init", &root_arg], None)
            .status
            .success()
    );
    let start = serde_json::json!({
        "work_item": {"New": {"source_kind": "Issue", "source_ref": "#53", "title": null, "goal": "cli"}},
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
    let WorkResponse::Started(started) =
        serde_json::from_slice(&output.stdout).expect("Started JSON")
    else {
        panic!("expected Started")
    };

    let raw: &[u8] = b"SECRET-RAW-53\0\xff\xfe\n\xc3\x28 not utf-8\r\nend without newline";
    let raw_file = home.path().join("raw.bin");
    fs::write(&raw_file, raw).expect("raw fixture");
    let mut input = result_input(
        started.working_state.work_item,
        vec![command(
            "test",
            &[&format!("cat:{}", raw_file.display()), "exit:1"],
            RAW,
        )],
    );
    input.outcome = WorkOutcomeWire::Complete;
    let output = run_cli(
        home.path(),
        &["work", "result", "--workspace", &root_arg, "--json"],
        Some(&serde_json::to_string(&input).expect("json")),
    );
    // A failed command with a capture still completes the WorkItem: #52.
    assert_eq!(output.status.code(), Some(0));
    let WorkResponse::Recorded(recorded) =
        serde_json::from_slice(&output.stdout).expect("Recorded JSON")
    else {
        panic!("expected Recorded")
    };
    let results = recorded.verification.expect("ran");
    assert_eq!(
        results[0].outcome,
        VerificationOutcomeWire::Failed { exit_code: 1 }
    );
    let handle = available(captured(&results[0]).2).handle.clone();

    let read = |args: &[&str]| {
        let mut all = vec!["artifact", "read"];
        all.extend_from_slice(args);
        run_cli(home.path(), &all, None)
    };
    let output = read(&[&handle, "--stream", "stdout", "--part", "head"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        output.stdout, raw,
        "stdout must be the raw bytes, nothing added"
    );

    let output = read(&[
        &handle,
        "--stream",
        "stdout",
        "--part",
        "head",
        "--max-bytes",
        "5",
        "--offset",
        "7",
    ]);
    assert_eq!(output.stdout, &raw[7..12]);

    let output = read(&[&handle, "--stream", "stdout", "--part", "head", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let response: ArtifactReadResponseWire =
        serde_json::from_slice(&output.stdout).expect("ArtifactRead JSON");
    assert_eq!(
        response,
        ArtifactReadResponseWire::Data {
            stream: OUT,
            part: HEAD,
            original_start_offset: 0,
            data_b64: STANDARD.encode(raw),
            returned_bytes: raw.len() as u32,
            next_offset: None,
        }
    );

    let output = read(&[&handle, "--stream", "stdout", "--part", "tail"]);
    assert_eq!((output.status.code(), output.stdout.len()), (Some(0), 0));

    let unknown = uuid::Uuid::new_v4().to_string();
    let output = read(&[&unknown, "--stream", "stdout", "--part", "head"]);
    assert_eq!((output.status.code(), output.stdout.len()), (Some(4), 0));

    for max_bytes in ["0", "524289", "lots"] {
        let output = read(&[
            &handle,
            "--stream",
            "stdout",
            "--part",
            "head",
            "--max-bytes",
            max_bytes,
        ]);
        assert_eq!(output.status.code(), Some(2), "--max-bytes {max_bytes}");
    }
    let output = read(&[&handle, "--stream", "both", "--part", "head"]);
    assert_eq!(output.status.code(), Some(2));

    // The daemon log names no output, handle or artifact path.
    let mut text = String::new();
    fs::File::open(&log)
        .and_then(|mut file| file.read_to_string(&mut text))
        .expect("daemon log");
    assert!(text.contains("verification test"), "{text}");
    for secret in ["SECRET-RAW-53", handle.as_str(), "artifacts"] {
        assert!(!text.contains(secret), "{secret} in the daemon log: {text}");
    }
}
