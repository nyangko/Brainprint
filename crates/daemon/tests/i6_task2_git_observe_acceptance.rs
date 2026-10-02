//! #51 (I6 task 2) acceptance: `"git": "Observe"` through the real daemon
//! protocol and the real CLI, against real Git repositories.
//!
//! Every failure is checked for "nothing written": the row counts of every
//! workspace.db work table are compared before and after.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{self, Child, Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use brainprint_core::{
    PROTOCOL_VERSION,
    protocol::{
        self, ClientConnection, HandshakeRequest, HandshakeResponse, InitRequest, InitResponse,
        Request, Response,
        query::{
            DirtyObservationWire, QueryErrorCodeWire, WorkItemSourceKindWire, WorkItemStatusWire,
            WorkspaceSelectorWire,
        },
        work::*,
    },
};
use brainprint_daemon::{runtime_paths, server::Server};
use brainprint_engine::{
    git_observation::{self, GitObservation},
    git_status,
    paths::{GlobalPaths, WorkspacePaths},
};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "brainprint-i6-task2-{label}-{}-{sequence}",
            process::id()
        ));
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

fn git(cwd: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A committed repository holding `src/app.rs` and `src/other.rs`.
fn git_repo(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("src");
    fs::write(root.join("src/app.rs"), "pub fn app() {}\n").expect("app.rs");
    fs::write(root.join("src/other.rs"), "pub fn other() {}\n").expect("other.rs");
    git(root, &["init", "-q"]);
    git(root, &["config", "core.autocrlf", "false"]);
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "init"]);
}

async fn send(connection: &mut ClientConnection, request: Request) -> Response {
    protocol::framing::write_message(connection, &request)
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
        Request::Handshake(HandshakeRequest {
            protocol_version,
            client_kind: "i6-task2-test".to_owned(),
        }),
    )
    .await
}

struct Fixture {
    _home: TestDir,
    _root: TestDir,
    connection: ClientConnection,
    paths: WorkspacePaths,
    _server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new(label: &str, is_git: bool) -> Self {
        let home = TestDir::create(&format!("{label}-home"));
        let root = TestDir::create(&format!("{label}-root"));
        if is_git {
            git_repo(root.path());
        } else {
            fs::write(root.path().join("plain.txt"), "x\n").expect("plain");
        }
        let global_paths = GlobalPaths::from_home(home.path());
        let mut server = Server::bind(&global_paths).await.expect("bind");
        let endpoint = runtime_paths::resolve(&global_paths);
        let server_task = tokio::spawn(async move {
            let _ = server.serve().await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut connection = connect(&endpoint).await;
        assert!(matches!(
            handshake(&mut connection, PROTOCOL_VERSION).await,
            Response::Handshake(HandshakeResponse::Ok { .. })
        ));
        let response = send(
            &mut connection,
            Request::Init(InitRequest {
                path: root.path().to_string_lossy().into_owned(),
            }),
        )
        .await;
        let Response::Init(InitResponse { workspace_root, .. }) = response else {
            panic!("expected Init, got {response:?}")
        };
        Self {
            _home: home,
            _root: root,
            connection,
            paths: WorkspacePaths::from_root(PathBuf::from(workspace_root)),
            _server: server_task,
        }
    }

    fn workspace_root(&self) -> &Path {
        &self.paths.workspace_root
    }

    async fn work(&mut self, operation: WorkOperationWire) -> WorkResponse {
        let request = Request::Work(WorkRequest {
            workspace: WorkspaceSelectorWire::Locator {
                path: self.workspace_root().to_string_lossy().into_owned(),
            },
            operation,
        });
        match send(&mut self.connection, request).await {
            Response::Work(response) => response,
            other => panic!("expected Work, got {other:?}"),
        }
    }

    async fn start(&mut self, head: Option<&str>, git: GitObservationWire) -> WorkResponse {
        self.work(WorkOperationWire::Start(WorkStartWire {
            work_item: WorkStartItemWire::New(NewWorkItemWire {
                source_kind: WorkItemSourceKindWire::Issue,
                source_ref: Some("#51".to_owned()),
                title: None,
                goal: "observe".to_owned(),
            }),
            head: head.map(str::to_owned),
            git,
            owner_agent: None,
        }))
        .await
    }

    /// Row counts of every work table: the "nothing written" witness.
    fn counts(&self) -> Vec<i64> {
        let connection = rusqlite::Connection::open_with_flags(
            &self.paths.workspace_db,
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
}

fn failed(response: WorkResponse) -> WorkErrorWire {
    match response {
        WorkResponse::Failed(failure) => {
            assert_eq!(failure.created_work_item, None);
            failure.error
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// The #50 dirty observation of what Git itself reports for `root`.
fn expected_dirty(root: &Path) -> DirtyObservationWire {
    match git_status::observe(root)
        .expect("direct observation")
        .observation
    {
        GitObservation::Clean => DirtyObservationWire::Clean,
        GitObservation::Dirty(entries) => DirtyObservationWire::Dirty {
            fingerprint: git_observation::fingerprint(&entries),
        },
        GitObservation::Unknown => panic!("an observation is never Unknown"),
    }
}

// ------------------------------------------------------------ round trip

#[tokio::test]
async fn observe_start_and_result_round_trip() {
    let mut fixture = Fixture::new("round-trip", true).await;
    let root = fixture.workspace_root().to_path_buf();
    fs::write(root.join("src/app.rs"), "pub fn app() { changed() }\n").expect("edit");
    fs::write(root.join("notes.txt"), "scratch\n").expect("untracked");
    let head = git(&root, &["rev-parse", "HEAD"]);
    let expected = expected_dirty(&root);
    assert!(matches!(expected, DirtyObservationWire::Dirty { .. }));

    let WorkResponse::Started(started) = fixture.start(None, GitObservationWire::Observe).await
    else {
        panic!("expected Started")
    };
    let state = &started.working_state;
    assert_eq!(state.baseline_head.as_deref(), Some(head.as_str()));
    assert_eq!(state.baseline_dirty, expected);
    // `.brainprint/` (created by init inside the repository) is excluded;
    // notes.txt is not indexed yet, so it stays unresolved.
    assert!(
        started
            .unresolved_paths
            .iter()
            .all(|path| !path.starts_with(".brainprint"))
    );

    // Restore the tree: the remaining state is observed CLEAN.
    git(&root, &["checkout", "--", "."]);
    fs::remove_file(root.join("notes.txt")).expect("remove");
    let work_item = state.work_item;
    let WorkResponse::Recorded(recorded) = fixture
        .work(WorkOperationWire::Result(WorkResultInputWire {
            work_item,
            outcome: WorkOutcomeWire::Complete,
            summary: "done".to_owned(),
            commit_id: None,
            verification_summary: None,
            verification: None,
            verification_job: None,
            git: GitObservationWire::Observe,
            change_set: None,
        }))
        .await
    else {
        panic!("expected Recorded")
    };
    assert_eq!(recorded.status, WorkItemStatusWire::Completed);
    assert_eq!(recorded.result.remaining_dirty, DirtyObservationWire::Clean);
}

// ---------------------------------------------------------- failure table

#[tokio::test]
async fn a_non_git_workspace_writes_nothing() {
    let mut fixture = Fixture::new("not-git", false).await;
    let before = fixture.counts();
    assert_eq!(
        failed(fixture.start(None, GitObservationWire::Observe).await),
        WorkErrorWire::GitObservation(GitObservationFailureWire::NotAGitWorkspace)
    );
    assert_eq!(fixture.counts(), before);
}

#[tokio::test]
async fn observe_with_a_caller_head_is_rejected_before_git_runs() {
    let mut fixture = Fixture::new("head-conflict", true).await;
    let before = fixture.counts();
    assert!(matches!(
        failed(
            fixture
                .start(Some("abc"), GitObservationWire::Observe)
                .await
        ),
        WorkErrorWire::InvalidObservation { .. }
    ));
    assert_eq!(fixture.counts(), before);
}

#[tokio::test]
async fn a_foreign_work_tree_is_a_boundary_mismatch() {
    let mut fixture = Fixture::new("boundary", true).await;
    let elsewhere = TestDir::create("boundary-elsewhere");
    let root = fixture.workspace_root().to_path_buf();
    git(
        &root,
        &[
            "config",
            "core.worktree",
            &elsewhere.path().to_string_lossy(),
        ],
    );
    let before = fixture.counts();
    assert_eq!(
        failed(fixture.start(None, GitObservationWire::Observe).await),
        WorkErrorWire::GitObservation(GitObservationFailureWire::WorkspaceBoundaryMismatch)
    );
    assert_eq!(fixture.counts(), before);
}

#[tokio::test]
async fn too_many_entries_write_nothing() {
    let mut fixture = Fixture::new("too-many", true).await;
    let root = fixture.workspace_root().to_path_buf();
    for index in 0..=git_observation::GIT_ENTRY_BOUND {
        fs::write(root.join(format!("u{index}.txt")), "x").expect("untracked");
    }
    let before = fixture.counts();
    assert_eq!(
        failed(fixture.start(None, GitObservationWire::Observe).await),
        WorkErrorWire::GitObservation(GitObservationFailureWire::TooManyEntries)
    );
    assert_eq!(fixture.counts(), before);
}

#[tokio::test]
async fn an_unregistered_workspace_fails_before_git_runs() {
    let mut fixture = Fixture::new("unregistered", true).await;
    let stranger = TestDir::create("unregistered-target");
    git_repo(stranger.path());
    let request = Request::Work(WorkRequest {
        workspace: WorkspaceSelectorWire::Locator {
            path: stranger.path().to_string_lossy().into_owned(),
        },
        operation: WorkOperationWire::Start(WorkStartWire {
            work_item: WorkStartItemWire::New(NewWorkItemWire {
                source_kind: WorkItemSourceKindWire::Issue,
                source_ref: None,
                title: None,
                goal: "never".to_owned(),
            }),
            head: None,
            git: GitObservationWire::Observe,
            owner_agent: None,
        }),
    });
    let Response::Work(response) = send(&mut fixture.connection, request).await else {
        panic!("expected Work")
    };
    assert!(matches!(
        failed(response),
        WorkErrorWire::Workspace(error) if error.code == QueryErrorCodeWire::NotInitialized
    ));
}

// --------------------------------------------------------------- version

#[tokio::test]
async fn protocol_version_is_pinned() {
    // #51 bumped it to 5, #52 to 6.
    assert_eq!(PROTOCOL_VERSION, 10);
}

/// v4 client → v5 daemon: refused at handshake; an Observe request sent
/// anyway is never served.
#[tokio::test]
async fn a_v4_client_is_refused_by_a_v5_daemon() {
    let fixture = Fixture::new("v4-client", true).await;
    let home = TestDir::create("v4-client-home");
    let global_paths = GlobalPaths::from_home(home.path());
    let mut server = Server::bind(&global_paths).await.expect("bind");
    let endpoint = runtime_paths::resolve(&global_paths);
    let _task = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let mut connection = connect(&endpoint).await;
    assert_eq!(
        handshake(&mut connection, 4).await,
        Response::Handshake(HandshakeResponse::VersionMismatch {
            server_protocol_version: PROTOCOL_VERSION,
            client_protocol_version: 4,
        })
    );
    let request = Request::Work(WorkRequest {
        workspace: WorkspaceSelectorWire::Locator {
            path: fixture.workspace_root().to_string_lossy().into_owned(),
        },
        operation: WorkOperationWire::Result(WorkResultInputWire {
            work_item: brainprint_core::WorkItemId::generate(),
            outcome: WorkOutcomeWire::Partial,
            summary: "never".to_owned(),
            commit_id: None,
            verification_summary: None,
            verification: None,
            verification_job: None,
            git: GitObservationWire::Observe,
            change_set: None,
        }),
    });
    let _ = protocol::framing::write_message(&mut connection, &request).await;
    let reply: std::io::Result<Response> = protocol::framing::read_message(&mut connection).await;
    assert!(
        reply.is_err(),
        "a mismatched client must not be served: {reply:?}"
    );
}

/// v5 client → v4 daemon: the client reports the mismatch and stops.
#[tokio::test]
async fn a_v5_client_stops_at_a_v4_daemon() {
    let home = TestDir::create("v4-daemon");
    let endpoint = runtime_paths::resolve(&GlobalPaths::from_home(home.path()));
    #[cfg(unix)]
    let listener = {
        fs::create_dir_all(endpoint.socket_path.parent().expect("parent")).expect("dir");
        protocol::Listener::bind(&endpoint.socket_path).expect("listener")
    };
    #[cfg(windows)]
    let listener = protocol::Listener::bind(&endpoint.pipe_name).expect("listener");
    let fake_v4 = tokio::spawn(async move {
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
                server_protocol_version: 4,
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
    let error = brainprint_daemon::client::handshake(&mut connection, "i6-task2-test")
        .await
        .expect_err("a v4 daemon must be refused");
    assert!(
        matches!(
            error,
            brainprint_daemon::client::ClientError::VersionMismatch {
                server_protocol_version: 4,
                client_protocol_version: PROTOCOL_VERSION,
            }
        ),
        "{error:?}"
    );
    assert_eq!(fake_v4.await.expect("fake daemon"), PROTOCOL_VERSION);
}

// ------------------------------------------------------------------ CLI

fn daemon_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_brainprintd"))
}

fn cli_bin() -> PathBuf {
    let mut path = daemon_bin();
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
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("XDG_RUNTIME_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("brainprint should run");
    {
        use std::io::Write as _;
        let mut input = child.stdin.take().expect("stdin");
        if let Some(text) = stdin {
            input.write_all(text.as_bytes()).expect("write stdin");
        }
    }
    child.wait_with_output().expect("output")
}

#[test]
fn the_cli_sends_observe_and_maps_git_failures_to_exit_4() {
    let home = TestDir::create("cli-home");
    let repo = TestDir::create("cli-repo");
    git_repo(repo.path());
    fs::write(
        repo.path().join("src/app.rs"),
        "pub fn app() { edited() }\n",
    )
    .expect("edit");
    let plain = TestDir::create("cli-plain");
    fs::write(plain.path().join("plain.txt"), "x\n").expect("plain");
    let _daemon = DaemonGuard(
        Command::new(daemon_bin())
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env_remove("XDG_RUNTIME_DIR")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("brainprintd"),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !run_cli(home.path(), &["status"], None).status.success() {
        assert!(Instant::now() < deadline, "daemon never became ready");
        thread::sleep(Duration::from_millis(50));
    }
    for root in [repo.path(), plain.path()] {
        let arg = root.to_string_lossy().into_owned();
        let init = run_cli(home.path(), &["init", &arg], None);
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
    }

    let start = r##"{"work_item":{"New":{"source_kind":"Issue","source_ref":"#51","title":null,"goal":"cli"}},"head":null,"git":"Observe","owner_agent":null}"##;
    let repo_arg = repo.path().to_string_lossy().into_owned();
    let output = run_cli(
        home.path(),
        &["work", "start", "--workspace", &repo_arg, "--json"],
        Some(start),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let WorkResponse::Started(started) =
        serde_json::from_slice(&output.stdout).expect("WorkResponse JSON")
    else {
        panic!("expected Started")
    };
    assert_eq!(
        started.working_state.baseline_head,
        Some(git(repo.path(), &["rev-parse", "HEAD"]))
    );
    assert!(matches!(
        started.working_state.baseline_dirty,
        DirtyObservationWire::Dirty { .. }
    ));

    let plain_arg = plain.path().to_string_lossy().into_owned();
    let output = run_cli(
        home.path(),
        &["work", "start", "--workspace", &plain_arg],
        Some(start),
    );
    assert_eq!(
        output.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("NotAGitWorkspace"));
}
