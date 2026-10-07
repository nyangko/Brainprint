//! #50 (I6 task 1) acceptance: caller-observed Git state → Working State
//! through the real daemon protocol (`Request::Work`) and the real CLI.
//!
//! Every failure-table row is checked for "nothing written": the row
//! counts of every workspace.db work table are compared before and after.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{self, Child, Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use brainprint_core::{
    PROTOCOL_VERSION, WorkItemId, WorkspaceId,
    protocol::{
        self, ClientConnection, HandshakeRequest, HandshakeResponse, InitRequest, InitResponse,
        Request, Response, query::*, work::*,
    },
};
use brainprint_daemon::{runtime_paths, server::Server};
use brainprint_engine::{
    generation::GenerationStore,
    git_observation::{self, GitEntry, GitEntryStatus},
    knowledge::{WorkResourceRole, WorkRuntime},
    paths::{GlobalPaths, WorkspacePaths},
};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "brainprint-i6-task1-{label}-{}-{sequence}",
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

// ------------------------------------------------------------ in-process

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
            client_kind: "i6-task1-test".to_owned(),
        }),
    )
    .await
}

/// A running daemon plus one initialized Workspace holding `src/app.rs`.
struct Fixture {
    _home: TestDir,
    _root: TestDir,
    connection: ClientConnection,
    workspace_id: WorkspaceId,
    paths: WorkspacePaths,
    _server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new(label: &str) -> Self {
        let home = TestDir::create(&format!("{label}-home"));
        let root = TestDir::create(&format!("{label}-root"));
        fs::create_dir_all(root.path().join("src")).expect("src");
        fs::write(root.path().join("src/app.rs"), "pub fn app() {}\n").expect("app.rs");
        fs::write(root.path().join("src/other.rs"), "pub fn other() {}\n").expect("other.rs");

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
        let Response::Init(InitResponse {
            workspace_id,
            workspace_root,
            ..
        }) = response
        else {
            panic!("expected Init, got {response:?}")
        };
        Self {
            _home: home,
            paths: WorkspacePaths::from_root(PathBuf::from(workspace_root)),
            _root: root,
            connection,
            workspace_id: workspace_id.parse().expect("workspace id"),
            _server: server_task,
        }
    }

    /// The canonical root init registered, as the CLI sends it.
    fn locator(&self) -> WorkspaceSelectorWire {
        WorkspaceSelectorWire::Locator {
            path: self.paths.workspace_root.to_string_lossy().into_owned(),
        }
    }

    async fn work(&mut self, operation: WorkOperationWire) -> WorkResponse {
        let request = Request::Work(WorkRequest {
            workspace: self.locator(),
            operation,
        });
        match send(&mut self.connection, request).await {
            Response::Work(response) => response,
            other => panic!("expected Work, got {other:?}"),
        }
    }

    async fn start(
        &mut self,
        work_item: WorkStartItemWire,
        git: GitObservationWire,
    ) -> WorkResponse {
        self.work(WorkOperationWire::Start(WorkStartWire {
            work_item,
            head: Some("0123abcd".to_owned()),
            git,
            owner_agent: Some("i6-task1".to_owned()),
        }))
        .await
    }

    async fn started(&mut self, goal: &str) -> WorkItemId {
        match self
            .start(new_item(goal), GitObservationWire::Unknown)
            .await
        {
            WorkResponse::Started(started) => started.working_state.work_item,
            other => panic!("expected Started, got {other:?}"),
        }
    }

    async fn result(
        &mut self,
        work_item: WorkItemId,
        outcome: WorkOutcomeWire,
        git: GitObservationWire,
        change_set: Option<Vec<GitEntryWire>>,
    ) -> WorkResponse {
        self.work(WorkOperationWire::Result(WorkResultInputWire {
            work_item,
            outcome,
            summary: "done".to_owned(),
            commit_id: Some("feedbeef".to_owned()),
            verification_summary: Some("cargo test: 3 passed".to_owned()),
            verification: None,
            verification_job: None,
            git,
            change_set,
        }))
        .await
    }

    async fn query(&mut self, operation: QueryOperationWire) -> QueryResultWire {
        let response = send(
            &mut self.connection,
            Request::Query(QueryRequest {
                request_id: uuid::Uuid::new_v4().to_string(),
                workspace: WorkspaceSelectorWire::Id {
                    workspace_id: self.workspace_id,
                },
                correlation: None,
                operation,
            }),
        )
        .await;
        match response {
            Response::Query(QueryResponse {
                outcome: QueryOutcomeWire::Ok(result),
                ..
            }) => result,
            other => panic!("expected an Ok query, got {other:?}"),
        }
    }

    async fn resume_evidence(&mut self, work_item: WorkItemId) -> Vec<EvidenceWire> {
        let result = self
            .query(QueryOperationWire::Context(ContextWire::Resume {
                work_item,
                target: None,
                scope_layers: Vec::new(),
                directives: Vec::new(),
                knowledge_refs: ProjectionKnowledgeRefsWire::default(),
                delivery: DeliveryWire {
                    budget: DeliveryBudgetWire {
                        max_items: std::num::NonZeroUsize::new(64),
                        max_bytes: std::num::NonZeroUsize::new(64 * 1024),
                    },
                    continuation: None,
                    retention: RetentionWire::Disabled,
                },
            }))
            .await;
        let QueryResultWire::Context(answer) = result else {
            panic!("expected Context, got {result:?}")
        };
        answer
            .page
            .evidence
            .into_iter()
            .filter_map(|item| match item {
                DeliveredItemWire::Full(evidence) => Some(evidence),
                DeliveredItemWire::Reuse(_) => None,
            })
            .collect()
    }

    /// Row counts of every work table: the "nothing written" witness.
    fn counts(&self) -> Vec<(&'static str, i64)> {
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
            let count = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .expect("count");
            (table, count)
        })
        .collect()
    }

    fn work_runtime(&self) -> WorkRuntime {
        WorkRuntime::open(
            self.workspace_id,
            &self.paths.workspace_db,
            &self.paths.index_db,
        )
        .expect("work runtime")
    }

    /// Wait for the daemon to publish the same stable-at-current state a
    /// query reads.
    async fn settled(&mut self) {
        self.query(QueryOperationWire::Find(FindQueryWire::Files {
            directory: None,
            recursive: true,
            path_prefix: None,
            role: None,
            language: None,
            kind: None,
            limit: std::num::NonZeroUsize::new(10).unwrap(),
        }))
        .await;
    }
}

fn new_item(goal: &str) -> WorkStartItemWire {
    WorkStartItemWire::New(NewWorkItemWire {
        source_kind: WorkItemSourceKindWire::Issue,
        source_ref: Some("#50".to_owned()),
        title: None,
        goal: goal.to_owned(),
    })
}

fn entry(status: GitEntryStatusWire, path: &str) -> GitEntryWire {
    GitEntryWire {
        status,
        path: path.to_owned(),
        old_path: None,
    }
}

fn failure(response: WorkResponse) -> WorkFailureWire {
    match response {
        WorkResponse::Failed(failure) => failure,
        other => panic!("expected Failed, got {other:?}"),
    }
}

// ------------------------------------------------------------ round trip

#[tokio::test]
async fn start_and_result_round_trip_through_the_existing_reads() {
    let mut fixture = Fixture::new("round-trip").await;
    fixture.settled().await;
    let entries = vec![
        entry(GitEntryStatusWire::Untracked, "scratch/"),
        entry(GitEntryStatusWire::Modified, "src/app.rs"),
        GitEntryWire {
            status: GitEntryStatusWire::Renamed,
            path: "src/renamed.rs".to_owned(),
            old_path: Some("src/old.rs".to_owned()),
        },
    ];
    let expected_fingerprint = git_observation::fingerprint(
        &git_observation::canonical_entries(&[
            GitEntry {
                path: "src/app.rs".to_owned(),
                old_path: None,
                status: GitEntryStatus::Modified,
            },
            GitEntry {
                path: "src/renamed.rs".to_owned(),
                old_path: Some("src/old.rs".to_owned()),
                status: GitEntryStatus::Renamed,
            },
            GitEntry {
                path: "scratch".to_owned(),
                old_path: None,
                status: GitEntryStatus::Untracked,
            },
        ])
        .expect("valid"),
    );

    let WorkResponse::Started(started) = fixture
        .start(
            new_item("round trip"),
            GitObservationWire::Dirty { entries },
        )
        .await
    else {
        panic!("expected Started")
    };
    assert_eq!(started.workspace_id, fixture.workspace_id);
    let work_item = started.working_state.work_item;
    assert_eq!(
        started.working_state.baseline_dirty,
        DirtyObservationWire::Dirty {
            fingerprint: expected_fingerprint.clone()
        }
    );
    assert_eq!(
        started.working_state.baseline_head.as_deref(),
        Some("0123abcd")
    );
    assert_eq!(started.unresolved_paths, vec!["scratch", "src/renamed.rs"]);

    // PREEXISTING_DIRTY exactly for the indexed path, via the engine's own
    // snapshot read (resume counts Work Resources but does not deliver
    // their rows).
    let snapshot = fixture
        .work_runtime()
        .snapshot(work_item, None)
        .expect("snapshot");
    let dirty: Vec<_> = snapshot
        .resources
        .iter()
        .filter(|resource| resource.role == WorkResourceRole::PreexistingDirty)
        .map(|resource| resource.locator_hint.clone())
        .collect();
    assert_eq!(dirty, vec![Some("src/app.rs".to_owned())]);

    // The existing resume read returns the stored baseline.
    let evidence = fixture.resume_evidence(work_item).await;
    assert!(evidence.iter().any(|item| matches!(
        item,
        EvidenceWire::WorkingState(state)
            if state.baseline_dirty == started.working_state.baseline_dirty
                && state.baseline_head.as_deref() == Some("0123abcd")
    )));

    // Partial: status stays ACTIVE.
    let WorkResponse::Recorded(partial) = fixture
        .result(
            work_item,
            WorkOutcomeWire::Partial,
            GitObservationWire::Unknown,
            None,
        )
        .await
    else {
        panic!("expected Recorded")
    };
    assert_eq!(partial.status, WorkItemStatusWire::Active);
    assert_eq!(partial.result.result_status, WorkResultStatusWire::Partial);
    assert_eq!(
        partial.result.remaining_dirty,
        DirtyObservationWire::Unknown
    );

    // Complete: CLEAN remains, change set fingerprinted order-free.
    let change_set = vec![
        entry(GitEntryStatusWire::Modified, "src/other.rs"),
        entry(GitEntryStatusWire::Added, "src/new.rs"),
    ];
    let mut reversed = change_set.clone();
    reversed.reverse();
    let WorkResponse::Recorded(done) = fixture
        .result(
            work_item,
            WorkOutcomeWire::Complete,
            GitObservationWire::Clean,
            Some(change_set),
        )
        .await
    else {
        panic!("expected Recorded")
    };
    assert_eq!(done.status, WorkItemStatusWire::Completed);
    assert_eq!(done.result.remaining_dirty, DirtyObservationWire::Clean);
    assert_eq!(done.result.commit_id.as_deref(), Some("feedbeef"));
    assert_eq!(
        done.result.verification_summary.as_deref(),
        Some("cargo test: 3 passed")
    );
    let change_fingerprint = done
        .result
        .change_set_fingerprint
        .clone()
        .expect("fingerprint");
    let reversed_entries: Vec<GitEntry> = reversed
        .into_iter()
        .map(|entry| GitEntry {
            path: entry.path,
            old_path: None,
            status: match entry.status {
                GitEntryStatusWire::Added => GitEntryStatus::Added,
                _ => GitEntryStatus::Modified,
            },
        })
        .collect();
    assert_eq!(
        change_fingerprint,
        git_observation::fingerprint(
            &git_observation::canonical_entries(&reversed_entries).expect("valid")
        )
    );

    let evidence = fixture.resume_evidence(work_item).await;
    assert!(evidence.iter().any(|item| matches!(
        item,
        EvidenceWire::WorkResult(result)
            if result.remaining_dirty == DirtyObservationWire::Clean
                && result.change_set_fingerprint.as_deref() == Some(change_fingerprint.as_str())
                && result.commit_id.as_deref() == Some("feedbeef")
    )));
}

#[tokio::test]
async fn a_second_start_can_name_an_existing_open_item_only() {
    let mut fixture = Fixture::new("existing").await;
    let work_item = fixture.started("first").await;
    let before = fixture.counts();
    // Already ACTIVE: InvalidTransition, baseline untouched.
    let failed = failure(
        fixture
            .start(
                WorkStartItemWire::Existing(work_item),
                GitObservationWire::Clean,
            )
            .await,
    );
    assert!(matches!(
        failed.error,
        WorkErrorWire::InvalidTransition { .. }
    ));
    assert_eq!(fixture.counts(), before);
    assert_eq!(
        fixture
            .work_runtime()
            .snapshot(work_item, None)
            .expect("snapshot")
            .working_state
            .expect("state")
            .baseline_head
            .as_deref(),
        Some("0123abcd")
    );
}

// ---------------------------------------------------------- failure table

#[tokio::test]
async fn malformed_observations_are_rejected_before_any_write() {
    let mut fixture = Fixture::new("malformed").await;
    let work_item = fixture.started("target").await;
    let before = fixture.counts();

    let rejected_git = [
        GitObservationWire::Dirty {
            entries: Vec::new(),
        },
        GitObservationWire::Dirty {
            entries: vec![entry(GitEntryStatusWire::Modified, "")],
        },
        GitObservationWire::Dirty {
            entries: vec![entry(GitEntryStatusWire::Modified, "/etc/passwd")],
        },
        GitObservationWire::Dirty {
            entries: vec![entry(GitEntryStatusWire::Modified, "../outside.rs")],
        },
        GitObservationWire::Dirty {
            entries: vec![entry(GitEntryStatusWire::Renamed, "no-old-path.rs")],
        },
        GitObservationWire::Dirty {
            entries: vec![GitEntryWire {
                status: GitEntryStatusWire::Modified,
                path: "a.rs".to_owned(),
                old_path: Some("b.rs".to_owned()),
            }],
        },
    ];
    for git in rejected_git {
        let failed = failure(fixture.start(new_item("never"), git.clone()).await);
        assert!(
            matches!(failed.error, WorkErrorWire::InvalidObservation { .. }),
            "{git:?}: {failed:?}"
        );
        assert_eq!(failed.created_work_item, None);
        let failed = failure(
            fixture
                .result(work_item, WorkOutcomeWire::Complete, git, None)
                .await,
        );
        assert!(matches!(
            failed.error,
            WorkErrorWire::InvalidObservation { .. }
        ));
    }
    let failed = failure(
        fixture
            .result(
                work_item,
                WorkOutcomeWire::Complete,
                GitObservationWire::Clean,
                Some(Vec::new()),
            )
            .await,
    );
    assert!(matches!(
        failed.error,
        WorkErrorWire::InvalidObservation { .. }
    ));

    // Over the bound: an error, never a truncated observation.
    let over: Vec<_> = (0..=git_observation::GIT_ENTRY_BOUND)
        .map(|index| entry(GitEntryStatusWire::Untracked, &format!("f{index}")))
        .collect();
    let failed = failure(
        fixture
            .start(
                new_item("never"),
                GitObservationWire::Dirty { entries: over },
            )
            .await,
    );
    assert!(matches!(failed.error, WorkErrorWire::BoundExceeded { .. }));

    assert_eq!(fixture.counts(), before, "nothing may be written");
}

#[tokio::test]
async fn unknown_and_terminal_work_items_are_typed_and_unchanged() {
    let mut fixture = Fixture::new("transitions").await;
    let work_item = fixture.started("finish me").await;
    assert!(matches!(
        fixture
            .result(
                work_item,
                WorkOutcomeWire::Complete,
                GitObservationWire::Clean,
                None
            )
            .await,
        WorkResponse::Recorded(_)
    ));
    let before = fixture.counts();

    for outcome in [
        WorkOutcomeWire::Partial,
        WorkOutcomeWire::Complete,
        WorkOutcomeWire::Abandon,
    ] {
        let failed = failure(
            fixture
                .result(work_item, outcome, GitObservationWire::Clean, None)
                .await,
        );
        assert!(
            matches!(failed.error, WorkErrorWire::InvalidTransition { .. }),
            "{outcome:?}: {failed:?}"
        );
    }

    let missing = WorkItemId::generate();
    let failed = failure(
        fixture
            .start(
                WorkStartItemWire::Existing(missing),
                GitObservationWire::Clean,
            )
            .await,
    );
    assert_eq!(
        failed.error,
        WorkErrorWire::WorkItemNotFound { work_item: missing }
    );
    for outcome in [WorkOutcomeWire::Partial, WorkOutcomeWire::Complete] {
        let failed = failure(
            fixture
                .result(missing, outcome, GitObservationWire::Clean, None)
                .await,
        );
        assert_eq!(
            failed.error,
            WorkErrorWire::WorkItemNotFound { work_item: missing }
        );
    }
    assert_eq!(fixture.counts(), before);
}

#[tokio::test]
async fn a_workspace_behind_its_stable_generation_creates_nothing() {
    let mut fixture = Fixture::new("not-ready").await;
    let before = fixture.counts();
    // Move the clock past the stable generation's basis, as a pending
    // refresh would. The Work path runs no refresh, so it stays behind.
    GenerationStore::open(&fixture.paths.index_db)
        .expect("index.db")
        .set_current_workspace_revision("i6-task1-ahead")
        .expect("advance clock");

    let failed = failure(
        fixture
            .start(
                new_item("never"),
                GitObservationWire::Dirty {
                    entries: vec![entry(GitEntryStatusWire::Modified, "src/app.rs")],
                },
            )
            .await,
    );
    assert!(
        matches!(
            &failed.error,
            WorkErrorWire::WorkspaceNotReady(WorkNotReadyWire::StableBehind { current, .. })
                if current == "i6-task1-ahead"
        ),
        "{failed:?}"
    );
    assert_eq!(failed.created_work_item, None);
    assert_eq!(
        fixture.counts(),
        before,
        "no OPEN WorkItem may be left behind"
    );
}

#[tokio::test]
async fn an_unregistered_workspace_is_a_workspace_error() {
    let mut fixture = Fixture::new("unregistered").await;
    let stranger = TestDir::create("unregistered-target");
    let request = Request::Work(WorkRequest {
        workspace: WorkspaceSelectorWire::Locator {
            path: stranger.path().to_string_lossy().into_owned(),
        },
        operation: WorkOperationWire::Start(WorkStartWire {
            work_item: new_item("never"),
            head: None,
            git: GitObservationWire::Unknown,
            owner_agent: None,
        }),
    });
    let Response::Work(response) = send(&mut fixture.connection, request).await else {
        panic!("expected Work")
    };
    let failed = failure(response);
    assert!(
        matches!(
            &failed.error,
            WorkErrorWire::Workspace(error) if error.code == QueryErrorCodeWire::NotInitialized
        ),
        "{failed:?}"
    );
}

/// A storage failure is `Internal` and leaves the stored rows as they
/// were. Unix-only: a read-only file is the portable way to force it.
#[cfg(unix)]
#[tokio::test]
async fn a_storage_failure_is_internal_and_writes_nothing() {
    use std::os::unix::fs::PermissionsExt as _;

    let mut fixture = Fixture::new("storage").await;
    let before = fixture.counts();
    let db = fixture.paths.workspace_db.clone();
    let original = fs::metadata(&db).expect("metadata").permissions();
    fs::set_permissions(&db, fs::Permissions::from_mode(0o444)).expect("read-only");

    let response = fixture
        .start(new_item("never"), GitObservationWire::Clean)
        .await;
    fs::set_permissions(&db, original).expect("restore");

    let failed = failure(response);
    assert!(
        matches!(failed.error, WorkErrorWire::Internal { .. }),
        "{failed:?}"
    );
    assert_eq!(fixture.counts(), before);
}

// --------------------------------------------------------------- version

#[tokio::test]
async fn the_protocol_version_is_pinned() {
    // #50 introduced v4; #51 bumped it to 5 (`Observe`), #52 to 6.
    assert_eq!(PROTOCOL_VERSION, 15);
}

/// Old client → new daemon: refused at handshake; a Work request sent
/// anyway is never served.
#[tokio::test]
async fn a_v3_client_is_refused_and_its_work_request_is_not_served() {
    let mut fixture = Fixture::new("v3-client").await;
    let home = TestDir::create("v3-client-home");
    let global_paths = GlobalPaths::from_home(home.path());
    let mut server = Server::bind(&global_paths).await.expect("bind");
    let endpoint = runtime_paths::resolve(&global_paths);
    let _task = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let mut connection = connect(&endpoint).await;

    assert_eq!(
        handshake(&mut connection, 3).await,
        Response::Handshake(HandshakeResponse::VersionMismatch {
            server_protocol_version: PROTOCOL_VERSION,
            client_protocol_version: 3,
        })
    );
    let request = Request::Work(WorkRequest {
        workspace: fixture.locator(),
        operation: WorkOperationWire::Start(WorkStartWire {
            work_item: new_item("never"),
            head: None,
            git: GitObservationWire::Unknown,
            owner_agent: None,
        }),
    });
    // The daemon closed the connection after the mismatch.
    let _ = protocol::framing::write_message(&mut connection, &request).await;
    let reply: std::io::Result<Response> = protocol::framing::read_message(&mut connection).await;
    assert!(
        reply.is_err(),
        "a mismatched client must not be served: {reply:?}"
    );
    // And the fixture's own (v4) connection still works.
    let _ = fixture.started("still served").await;
}

/// New client → old daemon: the client reports the mismatch and stops.
#[tokio::test]
async fn a_v4_client_stops_at_a_v3_daemon() {
    let home = TestDir::create("v3-daemon");
    let endpoint = runtime_paths::resolve(&GlobalPaths::from_home(home.path()));
    #[cfg(unix)]
    let listener = {
        fs::create_dir_all(endpoint.socket_path.parent().expect("parent")).expect("dir");
        protocol::Listener::bind(&endpoint.socket_path).expect("listener")
    };
    #[cfg(windows)]
    let listener = protocol::Listener::bind(&endpoint.pipe_name).expect("listener");
    let fake_v3 = tokio::spawn(async move {
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
                server_protocol_version: 3,
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
    let error = brainprint_daemon::client::handshake(&mut connection, "i6-task1-test")
        .await
        .expect_err("a v3 daemon must be refused");
    assert!(
        matches!(
            error,
            brainprint_daemon::client::ClientError::VersionMismatch {
                server_protocol_version: 3,
                client_protocol_version: PROTOCOL_VERSION,
            }
        ),
        "{error:?}"
    );
    assert_eq!(fake_v3.await.expect("fake daemon"), PROTOCOL_VERSION);
}

// ---------------------------------------------------------------- static

/// #50 boundary: the Work path runs no command and no Git.
#[test]
fn the_work_path_runs_no_command() {
    for (name, source) in [
        ("daemon work.rs", include_str!("../src/query/work.rs")),
        (
            "engine git_observation.rs",
            include_str!("../../engine/src/git_observation.rs"),
        ),
        ("cli work.rs", include_str!("../../cli/src/query/work.rs")),
    ] {
        let code = source.split("#[cfg(test)]").next().expect("code");
        for forbidden in ["Command::new", "process::", "\"git\"", "spawn"] {
            assert!(
                !code.contains(forbidden),
                "{name} must not reference {forbidden}"
            );
        }
    }
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
fn the_cli_sends_json_observations_and_maps_failures_to_exit_codes() {
    let home = TestDir::create("cli-home");
    let root = TestDir::create("cli-root");
    fs::create_dir_all(root.path().join("src")).expect("src");
    fs::write(root.path().join("src/app.rs"), "pub fn app() {}\n").expect("app.rs");
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
    let root_arg = root.path().to_string_lossy().into_owned();
    let init = run_cli(home.path(), &["init", &root_arg], None);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );

    let start = serde_json::to_string(&WorkStartWire {
        work_item: new_item("cli"),
        head: Some("abc".to_owned()),
        git: GitObservationWire::Dirty {
            entries: vec![
                entry(GitEntryStatusWire::Modified, "src/app.rs"),
                entry(GitEntryStatusWire::Untracked, "notes.txt"),
            ],
        },
        owner_agent: None,
    })
    .expect("json");
    let output = run_cli(
        home.path(),
        &["work", "start", "--workspace", &root_arg, "--json"],
        Some(&start),
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
    assert_eq!(started.unresolved_paths, vec!["notes.txt"]);
    let work_item = started.working_state.work_item;

    // From a file, human output.
    let input = root.path().join("result.json");
    fs::write(
        &input,
        serde_json::to_string(&WorkResultInputWire {
            work_item,
            outcome: WorkOutcomeWire::Complete,
            summary: "cli done".to_owned(),
            commit_id: None,
            verification_summary: None,
            verification: None,
            verification_job: None,
            git: GitObservationWire::Clean,
            change_set: None,
        })
        .expect("json"),
    )
    .expect("write input");
    let input_arg = input.to_string_lossy().into_owned();
    let output = run_cli(
        home.path(),
        &[
            "work",
            "result",
            "--workspace",
            &root_arg,
            "--input",
            &input_arg,
        ],
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("status Completed") && text.contains("remaining dirty: CLEAN"),
        "{text}"
    );

    // Exit codes: malformed JSON 2, invalid observation 2, terminal 4,
    // unregistered Workspace 3.
    let bad = run_cli(
        home.path(),
        &["work", "start", "--workspace", &root_arg],
        Some("{"),
    );
    assert_eq!(bad.status.code(), Some(2));
    let unknown_field = run_cli(
        home.path(),
        &["work", "start", "--workspace", &root_arg],
        Some(&start.replacen("\"head\"", "\"heads\"", 1)),
    );
    assert_eq!(unknown_field.status.code(), Some(2));
    let invalid = start.replace("src/app.rs", "../app.rs");
    let output = run_cli(
        home.path(),
        &["work", "start", "--workspace", &root_arg],
        Some(&invalid),
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = run_cli(
        home.path(),
        &[
            "work",
            "result",
            "--workspace",
            &root_arg,
            "--input",
            &input_arg,
        ],
        None,
    );
    assert_eq!(
        output.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stranger = TestDir::create("cli-stranger");
    let output = run_cli(
        home.path(),
        &[
            "work",
            "start",
            "--workspace",
            &stranger.path().to_string_lossy(),
        ],
        Some(&start),
    );
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
