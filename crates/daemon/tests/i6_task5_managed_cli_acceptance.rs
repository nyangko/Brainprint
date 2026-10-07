//! #54 step 3 acceptance: managed verification Jobs through protocol 8
//! and the real `brainprint verification start|poll|cancel` binary --
//! against a spawned `brainprintd`, or an in-process server where a test
//! needs its runtime (run counts, clean shutdown and restart).
//!
//! `harness = false`: this test binary is also the portable helper the
//! commands run (no shell). Helper ops, in order: `exit:<code>`,
//! `sleep:<ms>`, `mark:<path>` (append one byte: the file length counts
//! runs), `out:<n>` (n bytes on stdout), `line:<text>` (one stdout line),
//! `diags:<n>:<bytes>` (n distinct `path:line:column: message` lines),
//! `spawn [ ops… ]` (a child left running).

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
    PROTOCOL_VERSION, VerificationJobId,
    protocol::{
        self, ClientConnection, HandshakeRequest, HandshakeResponse, InitRequest, InitResponse,
        Request, Response, framing::MAX_MESSAGE_BYTES, query::WorkspaceSelectorWire,
        verification_job::*, work::*,
    },
};
use brainprint_daemon::{query::DaemonQueryRuntime, runtime_paths, server::Server};
use brainprint_engine::paths::GlobalPaths;
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
            "the_cli_returns_before_the_job_and_polls_it_to_the_end",
            the_cli_returns_before_the_job_and_polls_it_to_the_end,
        ),
        (
            "a_reconnected_start_replays_and_never_reruns",
            a_reconnected_start_replays_and_never_reruns,
        ),
        ("polls_are_durable_deltas", polls_are_durable_deltas),
        (
            "every_poll_fits_the_frame_and_pages_lose_nothing",
            every_poll_fits_the_frame_and_pages_lose_nothing,
        ),
        (
            "raw_handles_read_exactly_until_a_restart",
            raw_handles_read_exactly_until_a_restart,
        ),
        (
            "the_cli_cancels_the_tree_once",
            the_cli_cancels_the_tree_once,
        ),
        (
            "a_clean_restart_leaves_the_job_interrupted",
            a_clean_restart_leaves_the_job_interrupted,
        ),
        (
            "a_busy_workspace_creates_no_job",
            a_busy_workspace_creates_no_job,
        ),
        (
            "exit_codes_follow_the_typed_errors",
            exit_codes_follow_the_typed_errors,
        ),
        (
            "a_v14_client_is_refused_by_a_v15_daemon",
            a_v14_client_is_refused_by_a_v15_daemon,
        ),
        (
            "the_v15_cli_stops_at_a_v14_daemon",
            the_v15_cli_stops_at_a_v14_daemon,
        ),
        (
            "protocol_is_15_and_workspace_schema_7",
            protocol_is_15_and_workspace_schema_7,
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
            "diags" => {
                let (lines, bytes) = value.split_once(':').expect("n:bytes");
                let message = "m".repeat(bytes.parse().expect("bytes"));
                let mut stdout = std::io::stdout().lock();
                for line in 1..=lines.parse::<u32>().expect("lines") {
                    writeln!(stdout, "src/app.rs:{line}:1: {message}").expect("stdout");
                }
            }
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
        timeout_secs: 60,
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
    let sleep = format!("sleep:{LATE_MS}");
    batch(vec![
        command(
            "tree",
            &["spawn", "[", started, &sleep, late, "]", &sleep, late],
        ),
        command("after", &[late]),
    ])
}

/// How many times a helper wrote `path`.
fn count(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready() {
        assert!(Instant::now() < deadline, "{what} never happened");
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for(path: &Path) {
    wait_until(&format!("{} appearing", path.display()), || path.exists());
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

// ------------------------------------------------------------------ dirs

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "brainprint-i6-task5-cli-{label}-{}-{sequence}",
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

// ------------------------------------------------------------------- CLI

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

fn run_cli(home: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    run_cli_in(home, None, args, stdin)
}

fn run_cli_in(home: &Path, cwd: Option<&Path>, args: &[&str], stdin: Option<&str>) -> Output {
    let mut cli = Command::new(cli_bin());
    cli.env(brainprint_core::lifecycle::NO_AUTOSTART_ENV, "1");
    if let Some(cwd) = cwd {
        cli.current_dir(cwd);
    }
    let mut child = cli
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
        // A CLI that rejects its arguments exits without reading stdin;
        // writing after that is a broken pipe, not a test failure.
        if let Err(error) = input.write_all(text.as_bytes()) {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe, "write stdin");
        }
    }
    drop(input);
    child.wait_with_output().expect("output")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The CLI against one Workspace of one daemon home.
#[derive(Clone)]
struct Cli {
    home: PathBuf,
    /// Resolved by the CLI against `cwd` (else this process's).
    workspace: String,
    cwd: Option<PathBuf>,
}

impl Cli {
    fn run(&self, args: &[&str], stdin: Option<&str>) -> Output {
        run_cli_in(&self.home, self.cwd.as_deref(), args, stdin)
    }

    /// `verification <sub> … --workspace <ws> [--json]`.
    fn verification(&self, sub: &[&str], json: bool, stdin: Option<&str>) -> Output {
        let mut args = vec!["verification"];
        args.extend_from_slice(sub);
        args.extend_from_slice(&["--workspace", &self.workspace]);
        if json {
            args.push("--json");
        }
        self.run(&args, stdin)
    }

    fn start_output(&self, key: &str, verification: &VerificationWire, json: bool) -> Output {
        let input = serde_json::to_string(verification).expect("json");
        self.verification(
            &["start", "--idempotency-key", key, "--input", "-"],
            json,
            Some(&input),
        )
    }

    fn start(
        &self,
        key: &str,
        verification: &VerificationWire,
    ) -> (Option<i32>, VerificationJobStartResponseWire) {
        let output = self.start_output(key, verification, true);
        let response = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("start JSON: {}", text(&output.stderr)));
        (output.status.code(), response)
    }

    fn accepted(&self, key: &str, verification: &VerificationWire) -> VerificationJobId {
        let (code, response) = self.start(key, verification);
        let started = response.expect("accepted");
        assert_eq!(code, Some(0));
        assert!(!started.replayed);
        assert_eq!(started.state, VerificationJobStateWire::Running);
        assert_eq!(started.last_seq, 1);
        started.job_id
    }

    fn poll(
        &self,
        job: VerificationJobId,
        after: u64,
        limit: u32,
    ) -> (Option<i32>, VerificationJobPollResponseWire) {
        let (job, after, limit) = (job.to_string(), after.to_string(), limit.to_string());
        let output = self.verification(
            &["poll", &job, "--after", &after, "--limit", &limit],
            true,
            None,
        );
        let response = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("poll JSON: {}", text(&output.stderr)));
        (output.status.code(), response)
    }

    fn polled(&self, job: VerificationJobId, after: u64, limit: u32) -> VerificationJobPollWire {
        let (code, response) = self.poll(job, after, limit);
        assert_eq!(code, Some(0));
        response.expect("polled")
    }

    fn compact_poll(&self, job: VerificationJobId, after: u64) -> String {
        let (job, after) = (job.to_string(), after.to_string());
        let output = self.verification(&["poll", &job, "--after", &after], false, None);
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        text(&output.stdout)
    }

    fn cancel(&self, job: VerificationJobId) -> (Option<i32>, VerificationJobCancelResponseWire) {
        let output = self.verification(&["cancel", &job.to_string()], true, None);
        let response = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("cancel JSON: {}", text(&output.stderr)));
        (output.status.code(), response)
    }

    /// Every event of a Job once it is terminal.
    fn terminal(&self, job: VerificationJobId) -> VerificationJobPollWire {
        let mut done = None;
        wait_until("the Job ending", || {
            let poll = self.polled(job, 0, MAX_POLL_EVENTS);
            let ended = poll.state != VerificationJobStateWire::Running && !poll.has_more;
            done = ended.then_some(poll);
            ended
        });
        done.expect("terminal")
    }
}

fn seqs(poll: &VerificationJobPollWire) -> Vec<u64> {
    poll.events.iter().map(|event| event.seq).collect()
}

fn command_finished(poll: &VerificationJobPollWire, index: u32) -> &CommandFinishedWire {
    poll.events
        .iter()
        .find_map(|event| match &event.payload {
            VerificationJobEventPayloadWire::CommandFinished(finished)
                if finished.index == index =>
            {
                Some(finished)
            }
            _ => None,
        })
        .expect("COMMAND_FINISHED")
}

fn job_finished(poll: &VerificationJobPollWire) -> &[CommandResultWire] {
    match &poll.events.last().expect("events").payload {
        VerificationJobEventPayloadWire::JobFinished { results, .. } => results,
        other => panic!("expected JobFinished last, got {other:?}"),
    }
}

fn terminal_events(poll: &VerificationJobPollWire) -> usize {
    poll.events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                VerificationJobEventPayloadWire::JobFinished { .. }
                    | VerificationJobEventPayloadWire::JobCancelled { .. }
                    | VerificationJobEventPayloadWire::JobInterrupted { .. }
                    | VerificationJobEventPayloadWire::JobInternalError { .. }
            )
        })
        .count()
}

// ---------------------------------------------------------------- wire

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

async fn handshake(connection: &mut ClientConnection, version: u32) -> Response {
    send(
        connection,
        &Request::Handshake(HandshakeRequest {
            protocol_version: version,
            client_kind: "i6-task5-cli-test".to_owned(),
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

// --------------------------------------------------------------- daemons

/// A spawned `brainprintd` binary.
struct DaemonGuard(Child);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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

/// An in-process daemon with one initialized Git Workspace, driven by the
/// real CLI binary through the same endpoint.
struct Fixture {
    home: TestDir,
    _root: TestDir,
    global_paths: GlobalPaths,
    endpoint: runtime_paths::RuntimeEndpoint,
    cli: Cli,
    workspace_root: String,
    runtime: Arc<DaemonQueryRuntime>,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<Server>>,
}

impl Fixture {
    async fn new(label: &str) -> Self {
        let home = TestDir::create(&format!("{label}-home"));
        let root = TestDir::create(&format!("{label}-root"));
        workspace_tree(root.path());
        let global_paths = GlobalPaths::from_home(home.path());
        let (stop, server, runtime) = serve(&global_paths).await;
        let endpoint = runtime_paths::resolve(&global_paths);
        // Dropped at once: an open client would keep a Windows pipe
        // instance claimed across a restart.
        let workspace_root = {
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
            workspace_root
        };
        let cli = Cli {
            home: home.path().to_path_buf(),
            workspace: root.path().to_string_lossy().into_owned(),
            cwd: None,
        };
        Self {
            home,
            _root: root,
            global_paths,
            endpoint,
            cli,
            workspace_root,
            runtime,
            stop: Some(stop),
            server: Some(server),
        }
    }

    /// A marker path outside the Workspace (its writes never touch Git).
    fn marker(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    fn mark(&self, name: &str) -> String {
        format!("mark:{}", self.marker(name).display())
    }

    fn selector(&self) -> WorkspaceSelectorWire {
        WorkspaceSelectorWire::Locator {
            path: self.workspace_root.clone(),
        }
    }

    /// Clean shutdown, then a new daemon on the same home.
    async fn restart(&mut self) {
        self.stop
            .take()
            .expect("running")
            .send(())
            .expect("serving");
        let server = self.server.take().expect("running").await.expect("task");
        // #59: returns only once the endpoint is released, so the re-bind
        // below never meets a still-alive Windows pipe instance.
        server.close().await;
        let (stop, server, runtime) = serve(&self.global_paths).await;
        (self.stop, self.server, self.runtime) = (Some(stop), Some(server), runtime);
    }
}

// ----------------------------------------------------------------- tests

/// The real use: a spawned daemon, the CLI starts a long Job and returns
/// while it runs, then later polls see progress, new events only, and
/// the end.
fn the_cli_returns_before_the_job_and_polls_it_to_the_end() {
    let home = TestDir::create("real-home");
    let root = TestDir::create("real-root");
    workspace_tree(root.path());
    let _daemon = DaemonGuard(
        Command::new(env!("CARGO_BIN_EXE_brainprintd"))
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env_remove("XDG_RUNTIME_DIR")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("brainprintd"),
    );
    wait_until("the daemon", || {
        run_cli(home.path(), &["status"], None).status.success()
    });
    let root_arg = root.path().to_string_lossy().into_owned();
    let init = run_cli(home.path(), &["init", &root_arg], None);
    assert!(init.status.success(), "{}", text(&init.stderr));
    // `--workspace .` from the Workspace: the CLI makes it absolute.
    let cli = Cli {
        home: home.path().to_path_buf(),
        workspace: ".".to_owned(),
        cwd: Some(root.path().to_path_buf()),
    };
    let started = home.path().join("started");
    let done = home.path().join("done");
    let verification = batch(vec![
        command(
            "long",
            &[
                &format!("mark:{}", started.display()),
                "sleep:4000",
                &format!("mark:{}", done.display()),
            ],
        ),
        command("lint", &["exit:1"]),
        command("build", &["exit:0"]),
    ]);

    let began = Instant::now();
    let output = cli.start_output("real-use-1", &verification, false);
    let returned = began.elapsed();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let line = text(&output.stdout);
    let job: VerificationJobId = line
        .strip_prefix("job ")
        .and_then(|rest| rest.strip_suffix(" running\n"))
        .unwrap_or_else(|| panic!("start printed {line:?}"))
        .parse()
        .expect("job id");
    assert!(
        returned < Duration::from_millis(4000),
        "start waited for the command: {returned:?}"
    );
    assert!(!done.exists(), "the CLI returned after the long command");

    wait_for(&started);
    let progress = cli.polled(job, 0, MAX_POLL_EVENTS);
    assert_eq!(progress.state, VerificationJobStateWire::Running);
    assert!(!done.exists(), "the long command is still running");
    assert_eq!(
        progress.events[0].payload,
        VerificationJobEventPayloadWire::JobStarted
    );
    assert!(matches!(
        &progress.events[1].payload,
        VerificationJobEventPayloadWire::CommandStarted { index: 0, label } if label == "long"
    ));
    assert_eq!(seqs(&progress), [1, 2]);
    assert_eq!((progress.next_seq, progress.has_more), (2, false));
    let compact = cli.compact_poll(job, 0);
    assert_eq!(
        compact,
        format!("[1] job started\n[2] long started\njob {job} running next 2\n")
    );

    let done_poll = cli.terminal(job);
    let rest = cli.polled(job, progress.next_seq, MAX_POLL_EVENTS);
    assert_eq!(rest.state, VerificationJobStateWire::Finished);
    assert_eq!(rest.events, done_poll.events[2..]);
    assert_eq!(rest.events[0].seq, 3);
    let compact = cli.compact_poll(job, progress.next_seq);
    let lines: Vec<&str> = compact.lines().collect();
    assert!(!compact.contains("job started"), "{compact}");
    assert!(lines[0].starts_with("[3] long passed "), "{compact}");
    assert_eq!(lines[1], "[4] lint started");
    assert!(lines[2].starts_with("[5] lint failed exit=1 "), "{compact}");
    assert_eq!(lines[3], "[6] build skipped");
    assert_eq!(lines[4], "[7] job finished");
    // #55: one line for the Workspace after the batch; its markers are
    // outside the Workspace, so nothing changed.
    assert!(
        lines[5].starts_with("    workspace current revision ")
            && lines[5].ends_with("; changed 0"),
        "{compact}"
    );
    assert_eq!(lines[6], format!("job {job} finished next 7"));
    // Finished is "ran to its end": the failure is the command's fact.
    assert_eq!(
        command_finished(&done_poll, 1).outcome,
        VerificationOutcomeWire::Failed { exit_code: 1 }
    );
    assert_eq!(count(&started), 1);
    assert_eq!(count(&done), 1);
}

fn a_reconnected_start_replays_and_never_reruns() {
    block_on(async {
        let fixture = Fixture::new("replay").await;
        let cli = &fixture.cli;
        let runs = fixture.marker("runs");
        let verification = batch(vec![command(
            "once",
            &[&fixture.mark("runs"), "sleep:1500"],
        )]);
        let job = cli.accepted("replay-key", &verification);

        // The first CLI is gone; a second one retries with the same key.
        let (code, replay) = cli.start("replay-key", &verification);
        let replay = replay.expect("replayed");
        assert_eq!(code, Some(0));
        assert_eq!(
            (replay.job_id, replay.replayed, replay.state),
            (job, true, VerificationJobStateWire::Running)
        );
        let output = cli.start_output("replay-key", &verification, false);
        assert_eq!(
            text(&output.stdout),
            format!("job {job} running (replayed)\n")
        );

        cli.terminal(job);
        let (code, replay) = cli.start("replay-key", &verification);
        let replay = replay.expect("replayed");
        assert_eq!(code, Some(0));
        assert_eq!(
            (replay.job_id, replay.replayed, replay.state),
            (job, true, VerificationJobStateWire::Finished)
        );
        assert_eq!(replay.last_seq, 4);
        let output = cli.start_output("replay-key", &verification, false);
        assert_eq!(
            text(&output.stdout),
            format!("job {job} finished (replayed)\n")
        );
        assert_eq!(count(&runs), 1);

        let changed = batch(vec![command(
            "once",
            &[&fixture.mark("other"), "sleep:1500"],
        )]);
        let (code, conflict) = cli.start("replay-key", &changed);
        assert_eq!(code, Some(4));
        assert_eq!(conflict, Err(VerificationJobErrorWire::IdempotencyConflict));
        thread::sleep(Duration::from_millis(300));
        assert!(!fixture.marker("other").exists());
        assert_eq!(count(&runs), 1);
        assert_eq!(fixture.runtime.verification_runs(), 1);
    });
}

fn polls_are_durable_deltas() {
    block_on(async {
        let fixture = Fixture::new("delta").await;
        let cli = &fixture.cli;
        let verification = batch(
            ["a", "b", "c"]
                .into_iter()
                .map(|label| command(label, &[&fixture.mark(label)]))
                .collect(),
        );
        let job = cli.accepted("delta", &verification);
        let all = cli.terminal(job);
        assert_eq!(seqs(&all), (1..=8).collect::<Vec<_>>());

        let first = cli.polled(job, 0, 2);
        assert_eq!(
            (seqs(&first), first.next_seq, first.has_more),
            (vec![1, 2], 2, true)
        );
        let second = cli.polled(job, 2, 2);
        assert_eq!(
            (seqs(&second), second.next_seq, second.has_more),
            (vec![3, 4], 4, true)
        );
        assert_eq!(
            cli.polled(job, 2, 2),
            second,
            "the same cursor, the same delta"
        );
        assert_eq!(second.events, all.events[2..4]);

        let current = cli.polled(job, 8, MAX_POLL_EVENTS);
        assert!(current.events.is_empty());
        assert_eq!((current.next_seq, current.has_more), (8, false));
        assert_eq!(
            cli.compact_poll(job, 8),
            format!("job {job} finished next 8\n")
        );
        for label in ["a", "b", "c"] {
            assert_eq!(count(&fixture.marker(label)), 1, "{label} reran");
        }
        assert_eq!(fixture.runtime.verification_runs(), 1);
    });
}

/// The largest #53 diagnostics, 16 commands: every poll at every page
/// size encodes within one frame, and the pages put together are each
/// event exactly once, in order.
fn every_poll_fits_the_frame_and_pages_lose_nothing() {
    block_on(async {
        let fixture = Fixture::new("frame").await;
        let commands: Vec<_> = (0..16)
            .map(|index| {
                capturing(
                    &format!("lint-{index}"),
                    &["diags:64:2100"],
                    true,
                    Some(DiagnosticFormatWire::PathLineColumn),
                )
            })
            .collect();
        let job = fixture.cli.accepted("frame", &batch(commands));
        let all = fixture.cli.terminal(job);
        assert_eq!(all.events.len(), 34);
        let summaries: Vec<_> = job_finished(&all)
            .iter()
            .filter_map(|result| match &result.capture {
                CommandCaptureWire::Captured {
                    diagnostics: Some(summary),
                    ..
                } => Some(summary),
                _ => None,
            })
            .collect();
        assert_eq!(summaries.len(), 16);
        assert!(summaries.iter().any(|summary| summary.delivery_omitted > 0));

        let mut connection = open_connection(&fixture.endpoint).await;
        let mut largest = 0;
        for limit in [MAX_POLL_EVENTS, 1, 5, 33] {
            let mut after = 0;
            let mut seen = Vec::new();
            loop {
                let response = send(
                    &mut connection,
                    &Request::VerificationJobPoll(VerificationJobPollRequestWire {
                        workspace: fixture.selector(),
                        job_id: job,
                        after_seq: after,
                        limit,
                    }),
                )
                .await;
                let frame = serde_json::to_vec(&response).expect("encode").len();
                assert!(frame < MAX_MESSAGE_BYTES as usize, "{frame}");
                largest = largest.max(frame);
                let Response::VerificationJobPoll(Ok(page)) = response else {
                    panic!("expected a poll page")
                };
                seen.extend(page.events.iter().cloned());
                after = page.next_seq;
                if !page.has_more {
                    break;
                }
            }
            assert_eq!(seen, all.events, "limit {limit}");
        }
        assert!(largest > 256 * 1024, "not the largest shape: {largest}");
    });
}

fn raw_handles_read_exactly_until_a_restart() {
    block_on(async {
        let mut fixture = Fixture::new("raw").await;
        let cli = fixture.cli.clone();
        let job = cli.accepted(
            "raw",
            &batch(vec![capturing(
                "raw",
                &["line:src/app.rs:1:2: raw-boom", "out:3000"],
                true,
                Some(DiagnosticFormatWire::PathLineColumn),
            )]),
        );
        let before = cli.terminal(job);
        let RawAvailabilityWire::Available(reference) = (match &command_finished(&before, 0).capture
        {
            CaptureProgressWire::Captured { raw, .. } => raw.clone(),
            other => panic!("expected a capture, got {other:?}"),
        }) else {
            panic!("expected an available handle")
        };
        assert!(
            cli.compact_poll(job, 0)
                .contains(&format!("raw={}", reference.handle))
        );

        let read = cli.run(
            &[
                "artifact",
                "read",
                &reference.handle,
                "--stream",
                "stdout",
                "--part",
                "head",
            ],
            None,
        );
        assert_eq!(read.status.code(), Some(0));
        let mut expected = b"src/app.rs:1:2: raw-boom\n".to_vec();
        expected.extend_from_slice(&[b'o'; 3000]);
        assert_eq!(read.stdout, expected);

        fixture.restart().await;
        let after = cli.polled(job, 0, MAX_POLL_EVENTS);
        assert_eq!(after.state, VerificationJobStateWire::Finished);
        // Only the handles changed: the outcome, counts and diagnostics
        // are the durable record.
        let mut expected = before.clone();
        for event in &mut expected.events {
            match &mut event.payload {
                VerificationJobEventPayloadWire::CommandFinished(CommandFinishedWire {
                    capture: CaptureProgressWire::Captured { raw, .. },
                    ..
                }) => *raw = RawAvailabilityWire::Unavailable,
                VerificationJobEventPayloadWire::JobFinished { results, .. } => {
                    for result in results {
                        if let CommandCaptureWire::Captured { raw, .. } = &mut result.capture {
                            *raw = RawAvailabilityWire::Unavailable;
                        }
                    }
                }
                _ => {}
            }
        }
        assert_ne!(expected, before, "nothing was available");
        assert_eq!(after, expected);
        let read = cli.run(
            &[
                "artifact",
                "read",
                &reference.handle,
                "--stream",
                "stdout",
                "--part",
                "head",
            ],
            None,
        );
        assert_eq!(read.status.code(), Some(4));
    });
}

fn the_cli_cancels_the_tree_once() {
    block_on(async {
        let fixture = Fixture::new("cancel").await;
        let cli = &fixture.cli;
        let job = cli.accepted(
            "cancel",
            &slow_tree(&fixture.mark("started"), &fixture.mark("late")),
        );
        wait_for(&fixture.marker("started"));

        let output = cli.verification(&["cancel", &job.to_string()], false, None);
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        assert_eq!(text(&output.stdout), format!("job {job} cancelled\n"));
        let polled = cli.polled(job, 0, MAX_POLL_EVENTS);
        assert_eq!(polled.state, VerificationJobStateWire::Cancelled);
        assert_eq!(terminal_events(&polled), 1);
        // #55: the refresh after the cancelled tree comes with it.
        assert!(
            matches!(
                polled.events.last().expect("events").payload,
                VerificationJobEventPayloadWire::JobCancelled {
                    reason: JobEndReasonWire::CallerCancelled,
                    refresh: Some(PostCommandRefreshWire::Current { .. }),
                }
            ),
            "{:?}",
            polled.events.last()
        );

        let (code, again) = cli.cancel(job);
        assert_eq!(code, Some(0));
        assert_eq!(
            again,
            Ok(VerificationJobCancelledWire {
                job_id: job,
                state: VerificationJobStateWire::Cancelled
            })
        );
        assert_eq!(
            cli.polled(job, 0, MAX_POLL_EVENTS),
            polled,
            "a no-op recancel"
        );

        // The slot is free again.
        let next = cli.accepted("after-cancel", &batch(vec![command("ok", &["exit:0"])]));
        cli.terminal(next);

        thread::sleep(Duration::from_millis(LATE_MS + 1000));
        assert!(!fixture.marker("late").exists(), "the tree outlived cancel");
        assert_eq!(count(&fixture.marker("started")), 1);
    });
}

fn a_clean_restart_leaves_the_job_interrupted() {
    block_on(async {
        let mut fixture = Fixture::new("restart").await;
        let cli = fixture.cli.clone();
        let job = cli.accepted(
            "restart",
            &slow_tree(&fixture.mark("started"), &fixture.mark("late")),
        );
        wait_for(&fixture.marker("started"));

        fixture.restart().await;
        let polled = cli.polled(job, 0, MAX_POLL_EVENTS);
        assert_eq!(polled.state, VerificationJobStateWire::Interrupted);
        assert_eq!(terminal_events(&polled), 1);
        // #55: a shutdown never waits for the refresh.
        assert!(
            matches!(
                polled.events.last().expect("events").payload,
                VerificationJobEventPayloadWire::JobInterrupted {
                    reason: JobEndReasonWire::DaemonShutdown,
                    refresh: Some(PostCommandRefreshWire::DeferredDaemonShutdown { .. }),
                }
            ),
            "{:?}",
            polled.events.last()
        );
        let (code, cancelled) = cli.cancel(job);
        assert_eq!(code, Some(0));
        assert_eq!(
            cancelled.map(|cancelled| cancelled.state),
            Ok(VerificationJobStateWire::Interrupted)
        );

        thread::sleep(Duration::from_millis(LATE_MS + 1000));
        assert!(
            !fixture.marker("late").exists(),
            "the tree outlived shutdown"
        );
        assert_eq!(cli.polled(job, 0, MAX_POLL_EVENTS), polled);
        assert_eq!(
            fixture.runtime.verification_runs(),
            0,
            "a restart reran the Job"
        );
        assert_eq!(count(&fixture.marker("started")), 1);
    });
}

fn a_busy_workspace_creates_no_job() {
    block_on(async {
        let fixture = Fixture::new("busy").await;
        let cli = &fixture.cli;
        let long = cli.accepted("long", &batch(vec![command("long", &["sleep:1500"])]));
        let second = batch(vec![command("second", &[&fixture.mark("second")])]);

        let (code, busy) = cli.start("second", &second);
        assert_eq!(code, Some(4));
        assert_eq!(busy, Err(VerificationJobErrorWire::VerificationBusy));
        let output = cli.start_output("second", &second, false);
        assert_eq!(output.status.code(), Some(4));
        assert!(output.stdout.is_empty());
        assert!(text(&output.stderr).contains("no Job was created"));

        cli.terminal(long);
        assert!(!fixture.marker("second").exists());
        // The key is still free: no Job row was made for it.
        let accepted = cli.accepted("second", &second);
        cli.terminal(accepted);
        assert_eq!(count(&fixture.marker("second")), 1);
    });
}

fn exit_codes_follow_the_typed_errors() {
    block_on(async {
        let fixture = Fixture::new("exits").await;
        let cli = &fixture.cli;
        let never = batch(vec![command("never", &[&fixture.mark("never")])]);
        let code = |output: Output| output.status.code();

        let malformed = cli.verification(
            &["start", "--idempotency-key", "k", "--input", "-"],
            true,
            Some("{not json"),
        );
        assert_eq!(code(malformed), Some(2));
        let input = serde_json::to_string(&never).expect("json");
        let keyless = cli.verification(&["start", "--input", "-"], true, Some(&input));
        assert_eq!(code(keyless), Some(2));
        assert_eq!(
            cli.start("bad key", &never),
            (
                Some(2),
                Err(VerificationJobErrorWire::InvalidIdempotencyKey)
            )
        );
        let (exit, invalid) = cli.start("empty", &batch(Vec::new()));
        assert_eq!(exit, Some(2));
        assert!(matches!(
            invalid,
            Err(VerificationJobErrorWire::InvalidVerification { .. })
        ));

        let unknown = VerificationJobId::generate();
        assert_eq!(
            cli.poll(unknown, 0, 1),
            (Some(4), Err(VerificationJobErrorWire::JobNotFound))
        );
        assert_eq!(
            cli.cancel(unknown),
            (Some(4), Err(VerificationJobErrorWire::JobNotFound))
        );
        let job = cli.accepted("ok", &batch(vec![command("ok", &["exit:0"])]));
        cli.terminal(job);
        for limit in [0, MAX_POLL_EVENTS + 1] {
            assert_eq!(
                cli.poll(job, 0, limit),
                (Some(2), Err(VerificationJobErrorWire::InvalidLimit)),
                "limit {limit}"
            );
        }
        let output = cli.verification(&["poll", "not-a-job-id"], false, None);
        assert_eq!(code(output), Some(2));

        let elsewhere = TestDir::create("exits-uninitialized");
        let outside = Cli {
            home: cli.home.clone(),
            workspace: elsewhere.path().to_string_lossy().into_owned(),
            cwd: None,
        };
        let (exit, refused) = outside.start("k", &never);
        assert_eq!(exit, Some(3));
        assert!(matches!(
            refused,
            Err(VerificationJobErrorWire::Workspace(_))
        ));
        assert!(!fixture.marker("never").exists());
    });
}

/// v14 client → v15 daemon: refused at handshake; a start sent anyway is
/// never served and runs nothing.
fn a_v14_client_is_refused_by_a_v15_daemon() {
    block_on(async {
        let fixture = Fixture::new("v14-client").await;
        let mut connection = connect(&fixture.endpoint).await;
        assert_eq!(
            handshake(&mut connection, 14).await,
            Response::Handshake(HandshakeResponse::VersionMismatch {
                server_protocol_version: 15,
                client_protocol_version: 14,
            })
        );
        let request = Request::VerificationJobStart(VerificationJobStartRequestWire {
            workspace: fixture.selector(),
            idempotency_key: "v15".to_owned(),
            verification: batch(vec![command("never", &[&fixture.mark("never")])]),
        });
        let _ = protocol::framing::write_message(&mut connection, &request).await;
        let reply: std::io::Result<Response> =
            protocol::framing::read_message(&mut connection).await;
        assert!(reply.is_err(), "a mismatched client must not be served");
        thread::sleep(Duration::from_millis(300));
        assert!(!fixture.marker("never").exists());
        assert_eq!(fixture.runtime.verification_runs(), 0);
    });
}

/// The v15 CLI → a v14 daemon: the CLI reports the mismatch and stops.
fn the_v15_cli_stops_at_a_v14_daemon() {
    block_on(async {
        let home = TestDir::create("v14-daemon");
        let endpoint = runtime_paths::resolve(&GlobalPaths::from_home(home.path()));
        #[cfg(unix)]
        let listener = {
            fs::create_dir_all(endpoint.socket_path.parent().expect("parent")).expect("dir");
            protocol::Listener::bind(&endpoint.socket_path).expect("listener")
        };
        #[cfg(windows)]
        let listener = protocol::Listener::bind(&endpoint.pipe_name).expect("listener");
        let fake_v14 = tokio::spawn(async move {
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
                    server_protocol_version: 14,
                    client_protocol_version: handshake.protocol_version,
                }),
            )
            .await
            .expect("reply");
            handshake.protocol_version
        });
        let cli = Cli {
            home: home.path().to_path_buf(),
            workspace: home.path().to_string_lossy().into_owned(),
            cwd: None,
        };
        let job = VerificationJobId::generate().to_string();
        let output =
            tokio::task::spawn_blocking(move || cli.verification(&["poll", &job], false, None))
                .await
                .expect("cli");
        assert_eq!(output.status.code(), Some(5));
        assert!(
            text(&output.stderr).contains("protocol version mismatch"),
            "{}",
            text(&output.stderr)
        );
        assert_eq!(fake_v14.await.expect("fake daemon"), 15);
    });
}

fn protocol_is_15_and_workspace_schema_7() {
    assert_eq!(PROTOCOL_VERSION, 15);
    assert_eq!(
        brainprint_engine::schema::workspace::WORKSPACE_MIGRATIONS.len(),
        7
    );
}
