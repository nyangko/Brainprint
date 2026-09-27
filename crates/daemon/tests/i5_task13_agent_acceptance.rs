//! #26 (I5 Task 13) real-daemon adoption acceptance.
//!
//! The end-to-end half of the Task 13 suite: a real `Server` + populated
//! Workspace, real `brainprint-mcp` tool results (exactly what a client
//! reports in its post-tool payload), the real Claude Code bridge, the
//! common gateway, and the production `IpcProbe` over the real Task 11
//! local IPC. Lives in `brainprint-daemon` (which already depends on the
//! engine) and dev-depends on `brainprint-agent`/`brainprint-mcp` -- the
//! opposite of the forbidden direction.

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use brainprint_agent::{
    clients::ClientId,
    event::{DecisionKind, FallbackReason, Mode},
    gateway::{self, Outcome},
    probe::{IpcProbe, Probe as _},
    state::StateStore,
};
use brainprint_core::{
    PROTOCOL_VERSION,
    protocol::{
        self, ClientConnection, EndpointPaths, HandshakeRequest, InitRequest, InitResponse,
        Request, Response,
        query::{
            FindQueryWire, QueryOperationWire, QueryOutcomeWire, QueryRequest,
            WorkspaceSelectorWire,
        },
    },
};
use brainprint_daemon::server::Server;
use brainprint_engine::{
    config::WorkspaceConfig,
    paths::{GlobalPaths, WorkspacePaths},
    scan::BaselineScan,
};
use brainprint_mcp::{
    BrainprintMcp,
    tools::{find::FindParams, inspect::InspectParams},
};
use rmcp::handler::server::wrapper::Parameters;
use serde_json::{Value, json};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let path = env::temp_dir().join(format!(
            "bp-t13e2e-{label}-{}-{}",
            process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test dir");
        Self(path.canonicalize().expect("canonical"))
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const SHARED_TS: &str = "export function helper(): number {\n  return 42;\n}\n";
const APP_TS: &str = "import { helper } from \"./shared\";\n\nexport function run(): number {\n  return helper();\n}\n";

struct Fixture {
    _home: TestDir,
    workspace: TestDir,
    runtime: TestDir,
    paths: WorkspacePaths,
    endpoint: EndpointPaths,
    mcp: BrainprintMcp,
}

async fn fixture(label: &str) -> (Fixture, tokio::task::JoinHandle<()>) {
    let home = TestDir::create(&format!("{label}-home"));
    let global = GlobalPaths::from_home(&home.0);
    let mut server = Server::bind(&global).await.expect("bind");
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let endpoint = EndpointPaths::from_runtime_root(global.runtime_root());

    let workspace = TestDir::create(&format!("{label}-ws"));
    fs::create_dir_all(workspace.0.join("src")).expect("src");
    fs::write(workspace.0.join("src/shared.ts"), SHARED_TS).expect("shared");
    fs::write(workspace.0.join("src/app.ts"), APP_TS).expect("app");

    let mut connection = ClientConnection::connect(&endpoint.socket_path)
        .await
        .expect("connect");
    send(
        &mut connection,
        Request::Handshake(HandshakeRequest {
            protocol_version: PROTOCOL_VERSION,
            client_kind: "t13".into(),
        }),
    )
    .await;
    let Response::Init(InitResponse { .. }) = send(
        &mut connection,
        Request::Init(InitRequest {
            path: workspace.0.to_string_lossy().into_owned(),
        }),
    )
    .await
    else {
        panic!("init failed")
    };
    let paths = WorkspacePaths::from_root(&workspace.0);
    BaselineScan::open(&paths.index_db)
        .expect("index.db")
        .run_initial_scan(&workspace.0, &WorkspaceConfig::default(), "rev-1")
        .expect("baseline scan");

    let mcp = BrainprintMcp::with_endpoint(workspace.0.clone(), endpoint.clone());
    let fixture = Fixture {
        _home: home,
        runtime: TestDir::create(&format!("{label}-rt")),
        workspace,
        paths,
        endpoint,
        mcp,
    };
    (fixture, handle)
}

async fn send(connection: &mut ClientConnection, request: Request) -> Response {
    protocol::framing::write_message(connection, &request)
        .await
        .expect("write");
    protocol::framing::read_message(connection)
        .await
        .expect("read")
}

struct HookRun {
    outcome: Outcome,
    stdout: String,
    micros: u128,
    probes: u32,
}

impl Fixture {
    /// One real Claude Code hook invocation, through the bridge, the
    /// common gateway and the production IPC probe.
    async fn claude(&self, native_event: &'static str, payload: Value, mode: Mode) -> HookRun {
        let endpoint = self.endpoint.clone();
        let runtime = self.runtime.0.clone();
        tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let event = ClientId::ClaudeCode
                .normalize(native_event, &payload, None)
                .expect("normalize");
            let mut probe = IpcProbe::new(Some(endpoint));
            let outcome = gateway::handle(
                &event,
                mode,
                &StateStore::new(&runtime, "claude-code"),
                &mut probe,
            );
            let stdout = ClientId::ClaudeCode
                .render(native_event, &outcome.decision)
                .stdout;
            HookRun {
                outcome,
                stdout,
                micros: started.elapsed().as_micros(),
                probes: probe.stats().count,
            }
        })
        .await
        .expect("hook task")
    }

    fn tool(&self, tool: &str, input: Value, response: Option<Value>) -> Value {
        let mut payload = json!({
            "session_id": "e2e", "cwd": self.workspace.0, "hook_event_name": "PreToolUse",
            "tool_name": tool, "tool_input": input,
        });
        if let Some(response) = response {
            payload["tool_response"] = response;
        }
        payload
    }

    async fn native(&self, tool: &str, input: Value) -> HookRun {
        self.claude("PreToolUse", self.tool(tool, input, None), Mode::Guard)
            .await
    }

    async fn deliver(
        &self,
        tool: &str,
        input: Value,
        result: rmcp::model::CallToolResult,
    ) -> HookRun {
        let response = serde_json::to_value(&result).expect("CallToolResult json");
        self.claude(
            "PostToolUse",
            self.tool(tool, input, Some(response)),
            Mode::Guard,
        )
        .await
    }

    fn rescan(&self) {
        BaselineScan::open(&self.paths.index_db)
            .expect("index.db")
            .run_initial_scan(&self.workspace.0, &WorkspaceConfig::default(), "rev-1")
            .expect("rescan");
    }

    fn set_mode(&self, rel: &str, mode: u32) {
        fs::set_permissions(self.workspace.0.join(rel), fs::Permissions::from_mode(mode))
            .expect("chmod");
    }
}

fn params<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).expect("params")
}

#[tokio::test]
async fn e2e_real_daemon_exact_safe_matrix() {
    let (fixture, _server) = fixture("matrix").await;
    let mut hook_micros = Vec::new();
    let mut probe_counts = Vec::new();
    let mut note = |run: &HookRun| {
        hook_micros.push(run.micros);
        probe_counts.push(run.probes);
    };

    // ---- deliveries through the real MCP tools
    // A Resource target's CurrentSource can be a partial-line evidence
    // span (never a line-read basis); a declaration covers whole lines.
    let inspect_input = json!({"symbol_name": "run"});
    let inspected = fixture
        .mcp
        .inspect(Parameters(params::<InspectParams>(inspect_input.clone())))
        .await
        .expect("inspect");
    let run = fixture
        .deliver(
            "mcp__brainprint__brainprint_inspect",
            inspect_input,
            inspected,
        )
        .await;
    note(&run);
    let record = StateStore::new(&fixture.runtime.0, "claude-code").load("e2e");
    println!(
        "MEASURE e2e delivered_source_lines={:?}",
        record
            .sources
            .iter()
            .map(|source| source.lines)
            .collect::<Vec<_>>()
    );
    let source = record
        .sources
        .first()
        .expect("a real CurrentSource delivery was recorded")
        .clone();
    assert_eq!(source.path_rel, "src/app.ts");
    let (first, end) = source.lines;
    assert!(first < end);

    let files_input = json!({"mode": "files", "directory": "src", "recursive": true});
    let files = fixture
        .mcp
        .find(Parameters(params::<FindParams>(files_input.clone())))
        .await
        .expect("find files");
    note(
        &fixture
            .deliver("mcp__brainprint__brainprint_find", files_input, files)
            .await,
    );

    let text_input =
        json!({"mode": "text", "pattern": "helper", "regex": false, "path_prefix": "src/"});
    let text = fixture
        .mcp
        .find(Parameters(params::<FindParams>(text_input.clone())))
        .await
        .expect("find text");
    note(
        &fixture
            .deliver("mcp__brainprint__brainprint_find", text_input, text)
            .await,
    );

    let record = StateStore::new(&fixture.runtime.0, "claude-code").load("e2e");
    assert_eq!(
        (record.listings.len(), record.searches.len()),
        (1, 1),
        "files + text deliveries recorded"
    );
    let state_bytes = fs::read_dir(fixture.runtime.0.join("adoption/claude-code"))
        .expect("state")
        .map(|entry| entry.expect("entry").metadata().expect("meta").len())
        .max()
        .unwrap_or(0);
    let state_text: String = fs::read_dir(fixture.runtime.0.join("adoption/claude-code"))
        .expect("state")
        .map(|entry| fs::read_to_string(entry.expect("entry").path()).expect("read"))
        .collect();
    assert!(
        !state_text.contains("return helper()"),
        "no source body in state"
    );

    // ---- make every source file unreadable: any adapter source read
    // would now fail, yet every decision below must still be exact.
    fixture.set_mode("src/app.ts", 0o000);
    fixture.set_mode("src/shared.ts", 0o000);

    let sed =
        |from: usize, to: usize| json!({"command": format!("sed -n '{from},{to}p' src/app.ts")});
    let exact = fixture.native("Bash", sed(first + 1, end)).await;
    note(&exact);
    assert_eq!(
        exact.outcome.decision.kind,
        DecisionKind::SuppressRedirect,
        "12: exact delivered range"
    );
    assert!(exact.stdout.contains("\"permissionDecision\":\"deny\""));
    let read = fixture.native("Read", json!({"file_path": fixture.workspace.0.join("src/app.ts"), "offset": first + 1, "limit": end - first - 1})).await;
    note(&read);
    assert_eq!(
        read.outcome.decision.kind,
        DecisionKind::SuppressRedirect,
        "12: Read inside the delivered range"
    );

    let larger = fixture.native("Bash", sed(first + 1, end + 1)).await;
    note(&larger);
    assert_eq!(
        (
            larger.outcome.decision.kind,
            larger.outcome.decision.fallback
        ),
        (
            DecisionKind::Allow,
            Some(FallbackReason::RequestExceedsDeliveredRange)
        ),
        "13"
    );
    let whole = fixture
        .native(
            "Read",
            json!({"file_path": fixture.workspace.0.join("src/app.ts")}),
        )
        .await;
    assert_eq!(
        whole.outcome.decision.kind,
        DecisionKind::Allow,
        "13: whole file"
    );
    let other = fixture
        .native("Bash", json!({"command": "sed -n '1,2p' src/shared.ts"}))
        .await;
    assert_eq!(
        other.outcome.decision.kind,
        DecisionKind::Allow,
        "13: never-delivered file"
    );

    let glob = fixture
        .native(
            "Glob",
            json!({"pattern": "**/*", "path": fixture.workspace.0.join("src")}),
        )
        .await;
    note(&glob);
    assert_eq!(
        glob.outcome.decision.kind,
        DecisionKind::SuppressRedirect,
        "15: equivalent discovery"
    );
    let rg_files = fixture
        .native("Bash", json!({"command": "rg --files src"}))
        .await;
    assert_eq!(
        rg_files.outcome.decision.kind,
        DecisionKind::SuppressRedirect,
        "15"
    );
    let filtered = fixture
        .native(
            "Glob",
            json!({"pattern": "**/*.ts", "path": fixture.workspace.0.join("src")}),
        )
        .await;
    assert_eq!(
        filtered.outcome.decision.kind,
        DecisionKind::Allow,
        "16: filtered discovery"
    );

    let rg = fixture
        .native("Bash", json!({"command": "rg -l -F helper src"}))
        .await;
    note(&rg);
    assert_eq!(
        rg.outcome.decision.kind,
        DecisionKind::SuppressRedirect,
        "17: equivalent search"
    );
    let regex_grep = fixture
        .native(
            "Grep",
            json!({"pattern": "helper", "path": fixture.workspace.0.join("src")}),
        )
        .await;
    assert_eq!(
        regex_grep.outcome.decision.kind,
        DecisionKind::Allow,
        "regex vs literal: not equivalent"
    );
    let flagged = fixture
        .native(
            "Grep",
            json!({"pattern": "helper", "path": fixture.workspace.0.join("src"), "-C": 2}),
        )
        .await;
    assert_eq!(
        (
            flagged.outcome.decision.kind,
            flagged.outcome.decision.fallback
        ),
        (DecisionKind::Allow, Some(FallbackReason::UnsupportedFlags)),
        "18"
    );
    let piped = fixture
        .native(
            "Bash",
            json!({"command": format!("sed -n '{},{}p' src/app.ts | head -1", first + 1, end)}),
        )
        .await;
    assert_eq!(
        (piped.outcome.decision.kind, piped.outcome.decision.fallback),
        (
            DecisionKind::Allow,
            Some(FallbackReason::UnprovenEquivalence)
        ),
        "19"
    );

    // prefer never suppresses the same exact repeat.
    let prefer = fixture
        .claude(
            "PreToolUse",
            fixture.tool("Bash", sed(first + 1, end), None),
            Mode::Prefer,
        )
        .await;
    assert_eq!(prefer.outcome.decision.kind, DecisionKind::Advise, "7");

    // ---- a real revision change: edit + rescan -> every basis is stale.
    fixture.set_mode("src/app.ts", 0o644);
    fixture.set_mode("src/shared.ts", 0o644);
    fs::write(
        fixture.workspace.0.join("src/app.ts"),
        format!("{APP_TS}// helper touched\n"),
    )
    .expect("edit");
    fixture.rescan();
    let stale_read = fixture.native("Bash", sed(first + 1, end)).await;
    assert_eq!(
        (
            stale_read.outcome.decision.kind,
            stale_read.outcome.decision.fallback
        ),
        (DecisionKind::Allow, Some(FallbackReason::StaleOrNotCurrent)),
        "14"
    );
    let stale_glob = fixture
        .native(
            "Glob",
            json!({"pattern": "**/*", "path": fixture.workspace.0.join("src")}),
        )
        .await;
    assert_eq!(
        (
            stale_glob.outcome.decision.kind,
            stale_glob.outcome.decision.fallback
        ),
        (DecisionKind::Allow, Some(FallbackReason::StaleOrNotCurrent)),
        "14: listing"
    );
    let stale_rg = fixture
        .native("Bash", json!({"command": "rg -l -F helper src"}))
        .await;
    assert_eq!(
        (
            stale_rg.outcome.decision.kind,
            stale_rg.outcome.decision.fallback
        ),
        (DecisionKind::Allow, Some(FallbackReason::StaleOrNotCurrent)),
        "14: search scope"
    );

    // ---- Task 11/12 unaffected: the agent never acked, retained or
    // mutated anything; the same queries still answer normally.
    let mut connection = ClientConnection::connect(&fixture.endpoint.socket_path)
        .await
        .expect("connect");
    send(
        &mut connection,
        Request::Handshake(HandshakeRequest {
            protocol_version: PROTOCOL_VERSION,
            client_kind: "t13".into(),
        }),
    )
    .await;
    let Response::Query(response) = send(
        &mut connection,
        Request::Query(QueryRequest {
            request_id: "0b7f6c1e-8f5d-4d0a-9c1e-2a3b4c5d6e7f".into(),
            workspace: WorkspaceSelectorWire::Locator {
                path: fixture.workspace.0.to_string_lossy().into_owned(),
            },
            correlation: None,
            operation: QueryOperationWire::Find(FindQueryWire::Files {
                directory: None,
                recursive: true,
                path_prefix: None,
                role: None,
                language: None,
                kind: None,
                limit: std::num::NonZeroUsize::new(10).expect("nz"),
            }),
        }),
    )
    .await
    else {
        panic!("query")
    };
    assert!(matches!(response.outcome, QueryOutcomeWire::Ok(_)));
    assert!(response.ack_token.is_none());
    let again = fixture
        .mcp
        .inspect(Parameters(params::<InspectParams>(
            json!({"resource_path": "src/app.ts"}),
        )))
        .await
        .expect("inspect again");
    assert_ne!(again.is_error, Some(true), "12: Task 12 tool still answers");

    hook_micros.sort_unstable();
    println!(
        "MEASURE e2e hooks={} hook_us_p50={} hook_us_max={} probes_per_hook_max={} state_bytes_max={state_bytes} suppress_bytes={} ",
        hook_micros.len(),
        hook_micros[hook_micros.len() / 2],
        hook_micros.last().copied().unwrap_or(0),
        probe_counts.iter().max().copied().unwrap_or(0),
        exact.stdout.len(),
    );
    let _ = Path::new("");
}

#[tokio::test]
async fn e2e_uninitialized_or_unavailable_daemon_falls_back() {
    let (fixture, server) = fixture("fallback").await;
    // A directory the daemon never registered.
    let stranger = TestDir::create("stranger");
    fs::write(stranger.0.join("a.rs"), "fn a() {}\n").expect("write");
    let mut payload = fixture.tool("Bash", json!({"command": "rg -l -F a"}), None);
    payload["cwd"] = json!(stranger.0);
    let run = fixture
        .claude("PreToolUse", payload.clone(), Mode::Prefer)
        .await;
    assert_eq!(
        (run.outcome.decision.kind, run.outcome.decision.fallback),
        (DecisionKind::Allow, Some(FallbackReason::NotInitialized))
    );
    // Daemon gone.
    server.abort();
    let _ = server.await;
    payload["session_id"] = json!("after-stop");
    let run = fixture.claude("PreToolUse", payload, Mode::Guard).await;
    assert_eq!(run.outcome.decision.kind, DecisionKind::Allow);
    assert!(matches!(
        run.outcome.decision.fallback,
        Some(FallbackReason::DaemonUnavailable | FallbackReason::NotInitialized)
    ));
    println!("MEASURE daemon_down_hook_us={}", run.micros);
}
