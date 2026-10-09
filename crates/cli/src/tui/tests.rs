//! #70 acceptance: the reducer and renderer on their own, then the whole
//! TUI flow against an in-process daemon -- truth parity with the CLI's
//! rendering, unsupported-language honesty, locale, operations,
//! disconnect / restart / protocol mismatch.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use brainprint_core::{
    PROTOCOL_VERSION,
    present::{Locale, Msg, text},
    protocol::{
        EndpointPaths, HandshakeResponse, Listener, Request, Response, framing,
        maintenance::{StoredBasisWire, WorkspaceStatusWire},
        query::*,
        work::PostCommandRefreshWire,
    },
};
use brainprint_daemon::{query::DaemonQueryRuntime, runtime_paths, server::Server};
use brainprint_engine::paths::GlobalPaths;
use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
};

use super::{
    app::{App, Command, Connection, Daemon, Op, OpResult, Tab, perform},
    view,
};
use crate::client;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// The rendered screen at `width`x`height`, one string per row.
fn screen(app: &App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| view::render(frame, app))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

/// Whether `needle` is on screen. Spaces are ignored on both sides: a
/// wide (Korean) character fills two cells, the second one blank.
fn shows(app: &App, needle: &str) -> bool {
    let needle = needle.replace(' ', "");
    screen(app, 160, 60)
        .iter()
        .any(|row| row.replace(' ', "").contains(&needle))
}

// ------------------------------------------------------------ reducer

#[test]
fn only_y_runs_a_lifecycle_action_and_q_never_does() {
    let mut app = App::new(Locale::En);
    app.tab = Tab::Operations;
    app.op_selected = 2; // Rebuild
    assert_eq!(app.on_key(key(KeyCode::Enter)), None, "asks first");
    assert_eq!(app.confirm, Some(Op::Rebuild));
    assert!(shows(&app, text(Locale::En, Msg::ConfirmRebuildDetail)));
    // q / Enter / Esc inside the confirmation never run it.
    assert_eq!(app.on_key(key(KeyCode::Enter)), None);
    assert_eq!(app.on_key(key(KeyCode::Char('q'))), None);
    assert_eq!(app.confirm, None);
    assert!(!app.quit, "q cancels the dialog, it does not quit");
    app.on_key(key(KeyCode::Enter));
    assert_eq!(app.on_key(key(KeyCode::Esc)), None);
    assert_eq!(app.confirm, None);
    app.on_key(key(KeyCode::Enter));
    assert_eq!(
        app.on_key(key(KeyCode::Char('y'))),
        Some(Command::Run(Op::Rebuild))
    );

    app.op_selected = 3; // Uninit
    app.on_key(key(KeyCode::Enter));
    assert!(shows(&app, text(Locale::En, Msg::ConfirmUninitDetail)));
    app.on_key(key(KeyCode::Char('n')));

    // Doctor and sync need no confirmation; q at top level only quits.
    app.op_selected = 0;
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Command::Run(Op::Doctor))
    );
    assert_eq!(app.on_key(key(KeyCode::Char('q'))), None);
    assert!(app.quit);
}

#[test]
fn keys_move_between_views_search_and_help() {
    let mut app = App::new(Locale::Ko);
    assert_eq!(app.on_key(key(KeyCode::Tab)), None);
    assert_eq!(app.tab, Tab::Inspect);
    assert_eq!(app.on_key(key(KeyCode::BackTab)), Some(Command::Refresh));
    assert_eq!(app.tab, Tab::Overview);
    app.on_key(key(KeyCode::Char('2')));
    app.on_key(key(KeyCode::Char('/')));
    for character in "help".chars() {
        app.on_key(key(KeyCode::Char(character)));
    }
    // Typing `q` into the search field is text, not quit.
    app.on_key(key(KeyCode::Char('q')));
    assert!(!app.quit);
    app.on_key(key(KeyCode::Backspace));
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Command::Search("help".to_owned()))
    );
    app.on_key(key(KeyCode::Char('?')));
    assert!(shows(&app, text(Locale::Ko, Msg::HelpQuit)));
    app.on_key(key(KeyCode::Char('x')));
    assert!(!app.help);
    assert_eq!(
        app.on_key(key(KeyCode::Char('L'))),
        Some(Command::SwitchLocale)
    );
}

#[test]
fn every_view_draws_at_any_terminal_size() {
    let mut app = App::new(Locale::En);
    for tab in Tab::ALL {
        app.tab = tab;
        for (width, height) in [(80, 24), (120, 40), (40, 10), (12, 4), (1, 1)] {
            app.help = false;
            app.confirm = None;
            screen(&app, width, height);
            app.help = true;
            screen(&app, width, height);
            app.help = false;
            app.confirm = Some(Op::Uninit);
            screen(&app, width, height);
        }
    }
}

// --------------------------------------------------- against a daemon

static NEXT: AtomicU64 = AtomicU64::new(0);

pub(crate) struct TestDir(pub(crate) PathBuf);

impl TestDir {
    pub(crate) fn create(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "bp-tui-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("dir");
        Self(path.canonicalize().expect("canonical"))
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(crate) struct Running {
    handle: tokio::task::JoinHandle<()>,
    runtime: Arc<DaemonQueryRuntime>,
}

pub(crate) async fn start(home: &Path) -> (Running, EndpointPaths) {
    let global = GlobalPaths::from_home(home);
    let mut server = Server::bind(&global).await.expect("bind");
    let runtime = server.query_runtime();
    let endpoint = runtime_paths::resolve(&global);
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    (Running { handle, runtime }, endpoint)
}

pub(crate) async fn stop(running: Running) {
    running.handle.abort();
    let _ = running.handle.await;
    drop(running.runtime);
    tokio::time::sleep(Duration::from_millis(400)).await;
}

pub(crate) fn workspace() -> TestDir {
    let root = TestDir::create("ws");
    let src = root.0.join("src");
    fs::create_dir_all(&src).expect("src");
    fs::write(
        src.join("shared.ts"),
        "export function helper(): number {\n  return 42;\n}\n",
    )
    .expect("shared");
    fs::write(
        src.join("app.ts"),
        "import { helper } from \"./shared\";\n\nexport function run(): number {\n  return helper();\n}\n",
    )
    .expect("app");
    fs::write(
        src.join("main.go"),
        "package main\n\nfunc gopherOnly() int { return 1 }\n",
    )
    .expect("go");
    root
}

fn pick(app: &mut App, needle: &str) {
    app.selected = app
        .candidates
        .iter()
        .position(|candidate| candidate.label.contains(needle))
        .unwrap_or_else(|| panic!("no candidate {needle}: {:?}", app.candidates));
}

#[tokio::test]
async fn the_tui_shows_the_daemons_truth_and_outlives_nothing() {
    let home = TestDir::create("home");
    let root = workspace();
    let (running, endpoint) = start(&home.0).await;
    let mut connection = client::connect(&endpoint).await.expect("connect");
    let init = client::init(&mut connection, root.0.to_string_lossy().into_owned())
        .await
        .expect("init");
    drop(connection);

    let daemon = Daemon {
        endpoint: endpoint.clone(),
        workspace: root.0.to_string_lossy().into_owned(),
        config: None,
    };
    let mut app = App::new(Locale::En);

    // Overview: `status <path>` -- identity and the stored basis verbatim,
    // currentness as the active runtime holds it.
    perform(&mut app, &daemon, Command::Refresh).await;
    assert_eq!(app.connection, Connection::Connected);
    let overview = app.overview.as_ref().expect("overview");
    let Some(WorkspaceStatusWire::Initialized(report)) = &overview.status.workspace else {
        panic!("initialized: {:?}", overview.status)
    };
    assert_eq!(report.workspace_id, init.workspace_id);
    let StoredBasisWire::Stable(basis) = &report.basis else {
        panic!("a published basis: {:?}", report.basis)
    };
    let mut direct = client::connect(&endpoint).await.expect("connect");
    let cli_status = client::status(&mut direct, Some(daemon.workspace.clone()))
        .await
        .expect("status");
    drop(direct);
    assert_eq!(
        cli_status.workspace, overview.status.workspace,
        "TUI and CLI read one status"
    );
    assert!(shows(&app, &init.workspace_id) && shows(&app, &init.project_id));
    let generation_row = format!(
        "{}: {} ({} {})",
        text(Locale::En, Msg::LabelGeneration),
        basis.generation_no,
        text(Locale::En, Msg::LabelBasisRevision),
        basis.generation_basis_revision
    );
    assert!(shows(&app, &generation_row), "{:?}", screen(&app, 160, 60));
    assert!(shows(
        &app,
        &format!("Revision: {}", basis.workspace_revision)
    ));
    assert!(shows(&app, "Currentness: Current"));
    assert!(shows(&app, text(Locale::En, Msg::CapabilityPerQuery)));
    assert!(!shows(&app, text(Locale::En, Msg::MetricNotReported)));
    for (width, height) in [(80, 24), (120, 40), (30, 8)] {
        screen(&app, width, height);
    }

    // Locale: the words change, the facts and the stored answer do not.
    let status_before = overview.status.workspace.clone();
    perform(&mut app, &daemon, Command::SwitchLocale).await;
    assert_eq!(app.locale, Locale::Ko);
    assert_eq!(app.locale_notice, Some(Msg::LocaleNotSaved));
    assert!(shows(&app, &init.workspace_id));
    assert!(shows(&app, text(Locale::Ko, Msg::LabelWorkspace)));
    assert!(!shows(
        &app,
        &format!("{}:", text(Locale::En, Msg::LabelWorkspace))
    ));
    assert_eq!(
        app.overview.as_ref().expect("overview").status.workspace,
        status_before
    );
    assert!(
        shows(
            &app,
            &generation_row
                .replace(
                    &format!("{}:", text(Locale::En, Msg::LabelGeneration)),
                    &format!("{}:", text(Locale::Ko, Msg::LabelGeneration)),
                )
                .replace(
                    text(Locale::En, Msg::LabelBasisRevision),
                    text(Locale::Ko, Msg::LabelBasisRevision),
                )
        ),
        "same numbers in Korean"
    );
    perform(&mut app, &daemon, Command::SwitchLocale).await;
    assert_eq!(app.locale, Locale::En);

    // Inspect: search, choose, inspect -- the body is the CLI's rendering
    // of the same answer.
    app.tab = Tab::Inspect;
    perform(&mut app, &daemon, Command::Search("helper".to_owned())).await;
    pick(&mut app, "Function helper");
    perform(&mut app, &daemon, Command::Inspect).await;
    let inspected = app.inspect.as_ref().expect("inspect");
    assert_eq!(inspected.last.currentness, CurrentnessWire::Current);
    assert!(inspected.body.iter().any(|line| line.contains("return 42")));
    let target = app.target.clone().expect("target").target;
    let direct = daemon
        .query(QueryOperationWire::Inspect(InspectWire {
            target: target.clone(),
            delivery: crate::surface::delivery(None),
        }))
        .await
        .unwrap_or_else(|_| panic!("direct inspect"));
    let mut cli = Vec::new();
    crate::query::render::write_compact(&mut cli, &direct).expect("render");
    let cli: Vec<String> = String::from_utf8_lossy(&cli)
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(inspected.body, cli, "TUI body == CLI compact output");
    assert!(shows(&app, text(Locale::En, Msg::WorkspaceCurrent)));

    // Relations: counts and coverage as the daemon sent them.
    app.on_key(key(KeyCode::Tab));
    perform(&mut app, &daemon, Command::Relations).await;
    let relations = app.relations.clone().expect("relations");
    let direct = daemon
        .query(QueryOperationWire::Relations(RelationsWire {
            target: target.clone(),
            direction: RelationDirectionWire::Both,
            kinds: Vec::new(),
            delivery: crate::surface::delivery(None),
        }))
        .await
        .unwrap_or_else(|_| panic!("direct relations"));
    let QueryResultWire::Relations(direct) = direct else {
        panic!("relations")
    };
    assert_eq!(relations, direct, "same answer, nothing recomputed");
    let incoming = relations
        .totals
        .iter()
        .find(|totals| totals.direction == DirectionWire::Incoming)
        .expect("incoming");
    assert_eq!(incoming.confirmed, 1);
    assert!(
        shows(&app, "Confirmed 1 (Calls 1)"),
        "{:?}",
        screen(&app, 160, 60)
    );

    // Impact: the daemon's traversal, bounded.
    app.on_key(key(KeyCode::Tab));
    perform(&mut app, &daemon, Command::Impact).await;
    let impact = app.impact.as_ref().expect("impact");
    assert_eq!(impact.last.currentness, CurrentnessWire::Current);
    assert!(shows(&app, text(Locale::En, Msg::ImpactPublicSignature)));

    // An unsupported language: a Resource with source, Unsupported
    // relations -- never "None (complete coverage)".
    app.tab = Tab::Inspect;
    perform(&mut app, &daemon, Command::Search("src/main.go".to_owned())).await;
    pick(&mut app, "src/main.go");
    perform(&mut app, &daemon, Command::Inspect).await;
    assert!(
        app.inspect
            .as_ref()
            .expect("inspect go")
            .body
            .iter()
            .any(|line| line.contains("Unsupported")),
        "{:?}",
        app.inspect.as_ref().map(|paged| &paged.body)
    );
    app.tab = Tab::Relations;
    perform(&mut app, &daemon, Command::Relations).await;
    let outgoing = app
        .relations
        .as_ref()
        .expect("go relations")
        .totals
        .iter()
        .find(|totals| totals.direction == DirectionWire::Outgoing)
        .expect("outgoing")
        .clone();
    assert_eq!(
        outgoing.coverage.scope.as_ref().map(|scope| scope.support),
        Some(SupportWire::Unsupported)
    );
    assert!(
        shows(&app, "Outgoing: Unsupported"),
        "{:?}",
        screen(&app, 160, 60)
    );
    assert!(!shows(&app, "Outgoing: None (complete coverage)"));

    // Operations: doctor, sync, rebuild through the confirmation.
    app.tab = Tab::Operations;
    perform(&mut app, &daemon, Command::Run(Op::Doctor)).await;
    assert!(matches!(app.op_result, Some(OpResult::Doctor(_))));
    perform(&mut app, &daemon, Command::Run(Op::Sync)).await;
    let Some(OpResult::Sync(sync)) = &app.op_result else {
        panic!("sync: {:?}", app.error)
    };
    assert!(matches!(
        sync.refresh,
        PostCommandRefreshWire::Current { .. }
    ));
    assert!(
        app.inspect.is_none() && app.relations.is_none(),
        "old basis dropped"
    );
    app.op_selected = 2;
    app.on_key(key(KeyCode::Enter));
    let command = app.on_key(key(KeyCode::Char('y'))).expect("confirmed");
    perform(&mut app, &daemon, command).await;
    let Some(OpResult::Rebuild(rebuild)) = &app.op_result else {
        panic!("rebuild: {:?}", app.error)
    };
    assert_eq!(rebuild.workspace_id, init.workspace_id);
    assert!(root.0.join("src/shared.ts").is_file(), "source untouched");

    // Quitting the TUI is dropping it: the daemon still answers.
    drop(app);
    let mut connection = client::connect(&endpoint).await.expect("daemon alive");
    client::status(&mut connection, None).await.expect("status");
    drop(connection);

    // Disconnect: every earlier answer goes, the screen says so.
    let mut app = App::new(Locale::En);
    perform(&mut app, &daemon, Command::Refresh).await;
    stop(running).await;
    perform(&mut app, &daemon, Command::Refresh).await;
    assert!(matches!(app.connection, Connection::Disconnected(_)));
    assert!(app.overview.is_none());
    assert!(shows(&app, text(Locale::En, Msg::ConnectionCleared)));
    assert!(!shows(&app, &init.workspace_id));

    // A restarted daemon is reached by the next refresh.
    let (running, _) = start(&home.0).await;
    perform(&mut app, &daemon, Command::Refresh).await;
    assert_eq!(app.connection, Connection::Connected);
    assert!(shows(&app, &init.workspace_id));

    // Uninit through its confirmation: detached, then nothing claimed.
    app.tab = Tab::Operations;
    app.op_selected = 3;
    app.on_key(key(KeyCode::Enter));
    let command = app.on_key(key(KeyCode::Char('y'))).expect("confirmed");
    perform(&mut app, &daemon, command).await;
    let Some(OpResult::Uninit(uninit)) = &app.op_result else {
        panic!("uninit: {:?}", app.error)
    };
    assert!(!uninit.already_detached);
    assert!(root.0.join("src/shared.ts").is_file());
    assert!(root.0.join(".brainprint").is_dir(), "detach, not erase");
    // The refreshed overview states what `status` states: detached.
    let Some(overview) = &app.overview else {
        panic!("overview after uninit")
    };
    let Some(WorkspaceStatusWire::NotInitialized { reason, .. }) = &overview.status.workspace
    else {
        panic!("detached: {:?}", overview.status.workspace)
    };
    assert!(reason.contains("WorkspaceDetached"), "{reason}");
    app.tab = Tab::Overview;
    assert!(shows(&app, text(Locale::En, Msg::WorkspaceNotInitialized)));
    stop(running).await;
}

#[tokio::test]
async fn a_protocol_mismatch_is_incompatible_and_names_the_stale_side() {
    let runtime_root = TestDir::create("mismatch");
    let endpoint = EndpointPaths::from_runtime_root(runtime_root.0.clone());
    #[cfg(unix)]
    let mut listener = {
        // A long runtime root falls back to a short socket path elsewhere.
        fs::create_dir_all(endpoint.socket_path.parent().expect("parent")).expect("dir");
        Listener::bind(&endpoint.socket_path).expect("listener")
    };
    #[cfg(windows)]
    let mut listener = Listener::bind(&endpoint.pipe_name).expect("listener");
    let fake = tokio::spawn(async move {
        let mut connection = listener.accept().await.expect("accept");
        let Request::Handshake(handshake) = framing::read_message(&mut connection)
            .await
            .expect("handshake")
        else {
            panic!("handshake first")
        };
        framing::write_message(
            &mut connection,
            &Response::Handshake(HandshakeResponse::VersionMismatch {
                server_protocol_version: PROTOCOL_VERSION - 1,
                client_protocol_version: handshake.protocol_version,
            }),
        )
        .await
        .expect("reply");
    });
    let daemon = Daemon {
        endpoint,
        workspace: runtime_root.0.to_string_lossy().into_owned(),
        config: None,
    };
    let mut app = App::new(Locale::En);
    perform(&mut app, &daemon, Command::Refresh).await;
    fake.await.expect("fake daemon");
    let Connection::Incompatible(message) = &app.connection else {
        panic!("{:?}", app.connection)
    };
    assert!(
        message.contains("the running brainprintd is older"),
        "{message}"
    );
    assert!(app.overview.is_none());
    assert!(shows(&app, text(Locale::En, Msg::ConnectionIncompatible)));
}

/// #85 on the public surfaces: a real text search through the daemon,
/// read as the TUI/Web body (`surface::compact`) -- the same lines the
/// CLI prints -- names its status and what limited the scan.
#[tokio::test]
async fn text_search_bodies_name_their_status_and_coverage() {
    let home = TestDir::create("home-text");
    let root = workspace();
    let (_running, endpoint) = start(&home.0).await;
    let mut connection = client::connect(&endpoint).await.expect("connect");
    client::init(&mut connection, root.0.to_string_lossy().into_owned())
        .await
        .expect("init");
    drop(connection);
    let daemon = Daemon {
        endpoint,
        workspace: root.0.to_string_lossy().into_owned(),
        config: None,
    };

    let search = |literal: &str, max_results: usize, max_bytes: u64, max_file_bytes: u64| {
        QueryOperationWire::Find(FindQueryWire::Text {
            pattern: TextPatternWire::Literal(literal.to_owned()),
            case_insensitive: false,
            path_prefix: None,
            search_budget: SearchBudgetWire {
                max_results,
                max_files: 500,
                max_bytes,
                deadline_ms: None,
            },
            max_file_bytes,
            with_preview: true,
        })
    };
    let wide = 1024 * 1024;
    let cases = [
        // Complete: matches only / "no matches", no invented warning.
        (search("helper", 50, wide, wide), "Found", None),
        (search("no such needle", 50, wide, wide), "NotFound", None),
        // Each budget axis and an oversized text file, with and without hits.
        (
            search("no such needle", 50, 1, wide),
            "Truncated",
            Some("budget Bytes"),
        ),
        (
            search("helper", 1, wide, wide),
            "Truncated",
            Some("budget Results"),
        ),
        (
            search("no such needle", 50, wide, 10),
            "Truncated",
            Some("oversized 3"),
        ),
        (search("helper", 50, wide, 60), "Found", Some("oversized 1")),
    ];
    for (operation, status, limit) in cases {
        let result = daemon
            .query(operation)
            .await
            .unwrap_or_else(|_| panic!("text search"));
        let QueryResultWire::Find(FindResultWire::Text(text)) = &result else {
            panic!("a text result")
        };
        assert_eq!(format!("{:?}", text.status), status);
        let body = crate::surface::compact(&result);
        let mut cli = Vec::new();
        crate::query::render::write_compact(&mut cli, &result).expect("render");
        assert_eq!(
            body.join("\n") + "\n",
            String::from_utf8(cli).expect("utf-8"),
            "TUI/Web body == CLI compact output"
        );
        let coverage: Vec<&String> = body
            .iter()
            .filter(|line| line.starts_with("coverage Partial:"))
            .collect();
        match limit {
            None => assert!(coverage.is_empty(), "{status}: {body:?}"),
            Some(limit) => {
                assert_eq!(coverage.len(), 1, "{body:?}");
                assert!(coverage[0].contains(limit), "{body:?}");
                assert!(!body.iter().any(|line| line == "no matches"), "{body:?}");
            }
        }
        match status {
            "Truncated" => assert_eq!(body[0], "TRUNCATED"),
            "NotFound" => assert_eq!(body, ["no matches"]),
            _ => assert!(
                body.iter()
                    .any(|line| line.starts_with("src/shared.ts:1: ")),
                "1-based hit line: {body:?}"
            ),
        }
    }
}
