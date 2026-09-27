//! #26 (I5 Task 13) focused adoption/substitution acceptance.
//!
//! Deterministic half of the Task 13 suite: the common gateway, the
//! bridges and the binary, driven with *real* Task 11 wire values
//! (`QueryResultWire` et al. from `brainprint-core`, serialized inside a
//! real Task 12 envelope shape) and a deterministic in-memory probe that
//! implements `Find::Files` semantics. The real-daemon half lives in
//! `crates/daemon/tests/i5_task13_agent_acceptance.rs`.
//!
//! Test names carry the #26 "Mandatory acceptance" item number.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::{self, Command, Stdio},
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime},
};

use brainprint_agent::{
    clients::ClientId,
    event::{
        ActionClass, BrainprintTool, ClientCapabilities, Decision, DecisionKind, EventKind,
        FallbackReason, IntegrationEvent, Mode, NativeAction, ResetSource, SearchPattern,
    },
    gateway::{self, BOOTSTRAP, Outcome},
    probe::{FilesProbe, IpcProbe, ListedResource, Listing, Probe, ProbeStats},
    state::{MAX_RECORD_BYTES, MAX_RECORDS_PER_CLIENT, StateStore},
};
use brainprint_core::{
    ResourceId, WorkspaceId,
    protocol::{EndpointPaths, query::*},
};
use serde_json::{Value, json};

// ------------------------------------------------------------- harness

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn create(label: &str) -> Self {
        let path = env::temp_dir().join(format!(
            "bp-t13-{label}-{}-{}",
            process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("temp dir");
        Self(path.canonicalize().expect("canonical temp dir"))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// An in-memory Workspace index answering `Find::Files` exactly like
/// `QueryIndex::list_files` (directory prefix, recursion, path prefix,
/// limit/truncation), plus switchable failure/currentness.
struct FakeIndex {
    root: String,
    workspace_id: WorkspaceId,
    current: bool,
    fail: Option<FallbackReason>,
    entries: Vec<ListedResource>,
    probes: u32,
}

#[derive(Clone)]
struct FakeProbe {
    index: Rc<RefCell<FakeIndex>>,
    stats: ProbeStats,
}

impl Probe for FakeProbe {
    fn files(&mut self, request: &FilesProbe) -> Result<Listing, FallbackReason> {
        self.stats.count += 1;
        self.index.borrow_mut().probes += 1;
        let index = self.index.borrow();
        if let Some(reason) = index.fail {
            return Err(reason);
        }
        if request.root != index.root {
            return Err(FallbackReason::NotInitialized);
        }
        // Task 11's own `Find::Files` cap (DEFAULT_CANDIDATE_LIMIT): a
        // larger request is an InvalidRequest from the real daemon.
        if request.limit > 200 {
            return Err(FallbackReason::BridgeError);
        }
        let directory_prefix = request
            .directory
            .as_deref()
            .map(|directory| format!("{}/", directory.trim_end_matches('/')));
        let mut entries = Vec::new();
        let mut truncated = false;
        for entry in &index.entries {
            if let Some(prefix) = &directory_prefix {
                if !entry.path_rel.starts_with(prefix.as_str()) {
                    continue;
                }
                if !request.recursive && entry.path_rel[prefix.len()..].contains('/') {
                    continue;
                }
            }
            if let Some(prefix) = &request.path_prefix
                && !entry.path_rel.starts_with(prefix.as_str())
            {
                continue;
            }
            if entries.len() == request.limit {
                truncated = true;
                break;
            }
            entries.push(entry.clone());
        }
        Ok(Listing {
            workspace_id: index.workspace_id,
            current: index.current,
            truncated,
            entries,
        })
    }

    fn stats(&self) -> ProbeStats {
        self.stats
    }
}

const APP_RS: &str =
    "pub fn run() -> u32 {\n    helper()\n}\n\npub fn helper() -> u32 {\n    42\n}\n";
/// A marker that must never reach state or telemetry.
const SECRET_SOURCE_MARKER: &str = "TOP-SECRET-SOURCE-BODY-7f3a";

struct Fixture {
    dir: TempDir,
    runtime: TempDir,
    root: String,
    ids: BTreeMap<String, ResourceId>,
    probe: FakeProbe,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let dir = TempDir::create(label);
        let runtime = TempDir::create(&format!("{label}-rt"));
        for (path, body) in [
            ("src/app.rs", APP_RS),
            ("src/lib.rs", "pub mod app;\n"),
            ("src/util/mod.rs", "pub fn budget() {}\n"),
            ("docs/readme.md", "# notes\n"),
        ] {
            let full = dir.0.join(path);
            fs::create_dir_all(full.parent().expect("parent")).expect("dirs");
            fs::write(full, body).expect("write");
        }
        let root = dir.0.to_string_lossy().into_owned();
        let mut ids = BTreeMap::new();
        let mut entries = Vec::new();
        for (path, is_file) in [
            ("docs", false),
            ("docs/readme.md", true),
            ("src", false),
            ("src/app.rs", true),
            ("src/lib.rs", true),
            ("src/util", false),
            ("src/util/mod.rs", true),
        ] {
            let id = ResourceId::generate();
            ids.insert(path.to_owned(), id);
            entries.push(ListedResource {
                id: id.to_string(),
                path_rel: path.to_owned(),
                revision: format!("rev1:{path}"),
                is_file,
            });
        }
        let probe = FakeProbe {
            index: Rc::new(RefCell::new(FakeIndex {
                root: root.clone(),
                workspace_id: WorkspaceId::generate(),
                current: true,
                fail: None,
                entries,
                probes: 0,
            })),
            stats: ProbeStats::default(),
        };
        Self {
            dir,
            runtime,
            root,
            ids,
            probe,
        }
    }

    fn store(&self, client: &str) -> StateStore {
        StateStore::new(&self.runtime.0, client)
    }

    fn abs(&self, rel: &str) -> String {
        self.dir.0.join(rel).to_string_lossy().into_owned()
    }

    fn revise(&self, path: &str) {
        let mut index = self.probe.index.borrow_mut();
        let entry = index
            .entries
            .iter_mut()
            .find(|entry| entry.path_rel == path)
            .expect("entry");
        entry.revision = format!("rev2:{path}");
    }

    fn event(
        &self,
        kind: EventKind,
        session: &str,
        action: Option<NativeAction>,
    ) -> IntegrationEvent {
        IntegrationEvent {
            kind,
            client_id: "claude-code".to_owned(),
            client_version: None,
            session_id: session.to_owned(),
            cwd: Some(self.root.clone()),
            reset_source: None,
            agent_id: None,
            can_inject_context: true,
            action,
            capabilities: ClientId::ClaudeCode.capabilities(),
        }
    }

    fn run(&self, event: &IntegrationEvent, mode: Mode) -> Outcome {
        let mut probe = self.probe.clone();
        gateway::handle(event, mode, &self.store(&event.client_id), &mut probe)
    }

    /// POST_TOOL of a Brainprint tool with the given result envelope.
    fn deliver(
        &self,
        session: &str,
        tool: BrainprintTool,
        input: Value,
        envelope: Value,
        mode: Mode,
    ) -> Outcome {
        let wrapped = json!([{ "type": "text", "text": envelope.to_string() }]);
        self.run(
            &self.event(
                EventKind::PostTool,
                session,
                Some(NativeAction::Brainprint {
                    tool,
                    input,
                    result: Some(wrapped),
                }),
            ),
            mode,
        )
    }

    fn pre(&self, session: &str, action: NativeAction, mode: Mode) -> Outcome {
        self.run(&self.event(EventKind::PreTool, session, Some(action)), mode)
    }

    // ----- real Task 11 wire values inside a real Task 12 envelope

    fn resource_wire(&self, path: &str) -> ResourceWire {
        let index = self.probe.index.borrow();
        let entry = index
            .entries
            .iter()
            .find(|entry| entry.path_rel == path)
            .expect("entry");
        ResourceWire {
            id: self.ids[path],
            path_rel: path.to_owned(),
            path_key: path.to_owned(),
            kind: if entry.is_file {
                ResourceKindWire::File
            } else {
                ResourceKindWire::Directory
            },
            role: ResourceRoleWire::Source,
            language: None,
            size_bytes: 1,
            mtime_ns: 1,
            fingerprint: "fp".to_owned(),
            content_hash: None,
            state: ResourceStateWire::Active,
            resource_revision: entry.revision.clone(),
            generated_kind: None,
            container_resource_id: None,
        }
    }

    fn listing_envelope(&self, directory: Option<&str>) -> Value {
        let prefix = directory.map(|directory| format!("{directory}/"));
        let paths: Vec<String> = self
            .probe
            .index
            .borrow()
            .entries
            .iter()
            .filter(|entry| {
                prefix
                    .as_ref()
                    .is_none_or(|prefix| entry.path_rel.starts_with(prefix.as_str()))
            })
            .map(|entry| entry.path_rel.clone())
            .collect();
        envelope(
            "brainprint.find",
            "files",
            &QueryResultWire::Find(FindResultWire::Files(FileListingWire {
                entries: paths.iter().map(|path| self.resource_wire(path)).collect(),
                truncated: false,
                currentness: CurrentnessWire::Current,
                source: ResultSourceWire::StructuralIndex,
            })),
        )
    }

    fn text_envelope(&self, hits: &[&str], status: QueryStatusWire) -> Value {
        envelope(
            "brainprint.find",
            "text",
            &QueryResultWire::Find(FindResultWire::Text(TextSearchResultWire {
                status,
                matches: hits
                    .iter()
                    .map(|path| TextMatchWire {
                        path_rel: (*path).to_owned(),
                        resource_id: Some(self.ids[*path]),
                        span: span(1, 0, 1, 6),
                        preview: Some(SECRET_SOURCE_MARKER.to_owned()),
                        source: MatchSourceWire::TextFallback,
                    })
                    .collect(),
                scope: ScopeReportWire::default(),
                structural_currentness: CurrentnessWire::Current,
                reason: FallbackReasonWire::ExplicitTextSearch,
            })),
        )
    }

    /// `inspect` delivering `src/app.rs` lines [start, end) (0-based).
    fn source_envelope(&self, path: &str, start: usize, end: usize) -> Value {
        let revision = self.resource_wire(path).resource_revision;
        envelope(
            "brainprint.inspect",
            "inspect",
            &QueryResultWire::Inspect(projected(vec![EvidenceWire::CurrentSource(
                PreparedRangeWire {
                    resource: self.ids[path],
                    path_rel: path.to_owned(),
                    resource_revision: revision,
                    span: span(start, 0, end, 0),
                    source: SECRET_SOURCE_MARKER.to_owned(),
                    role: RangeRoleWire::AnchorDeclaration,
                    verification: SourceVerificationWire {
                        expected_content_hash: "h".to_owned(),
                        observed_content_hash: "h".to_owned(),
                        currentness: CurrentnessWire::Current,
                    },
                },
            )])),
        )
    }
}

fn span(start_line: usize, start_col: usize, end_line: usize, end_col: usize) -> SourceSpanWire {
    SourceSpanWire {
        start_byte: 0,
        end_byte: 1,
        start: SourcePointWire {
            line: start_line,
            column: start_col,
        },
        end: SourcePointWire {
            line: end_line,
            column: end_col,
        },
    }
}

fn projected(evidence: Vec<EvidenceWire>) -> ProjectedAnswerWire {
    let zero = StageAmountWire {
        items: MeasureWire::Known(0),
        bytes: MeasureWire::Known(0),
        tokens: MeasureWire::NotMeasured,
    };
    ProjectedAnswerWire {
        target_resolution: TargetResolutionWire::NoTarget,
        currentness: CurrentnessWire::Current,
        page: DeliveryPageWire {
            references: vec![None; evidence.len()],
            evidence,
            gaps: Vec::new(),
            used_items: 1,
            used_bytes: 1,
        },
        economy: DeliveryEconomyWire {
            raw_available: zero,
            prepared: zero,
            delivered: zero,
            omitted_items: 0,
            more_available: false,
            limiting: Vec::new(),
            continuation_unavailable: None,
        },
        continuation: None,
        more_available: false,
    }
}

/// The exact Task 12 envelope shape (`brainprint-mcp::envelope::build`).
fn envelope(tool: &str, mode: &str, result: &QueryResultWire) -> Value {
    json!({
        "tool": tool, "mode": mode, "protocol_version": 1, "schema_version": 1,
        "outcome": "ok", "payload": result,
    })
}

fn read(path: String, lines: (usize, usize)) -> NativeAction {
    NativeAction::SourceRead {
        path,
        lines: Some(lines),
    }
}

fn kind(outcome: &Outcome) -> DecisionKind {
    outcome.decision.kind
}

/// Deliver `src/app.rs` lines [0,3) and return the fixture.
fn with_delivered_source(label: &str, mode: Mode) -> Fixture {
    let fixture = Fixture::new(label);
    fixture.deliver(
        "s1",
        BrainprintTool::Inspect,
        json!({"resource_path": "src/app.rs"}),
        fixture.source_envelope("src/app.rs", 0, 3),
        mode,
    );
    fixture
}

// ============================================================ Generic

#[test]
fn a01_builds_on_rust_1_88_is_declared() {
    let manifest = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("manifest");
    assert!(
        manifest.contains("rust-version.workspace = true"),
        "inherits the workspace MSRV (1.88)"
    );
}

fn agent_bin() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_brainprint-agent"));
    // #26 acceptance 53: valid with every reference product absent.
    command.env("PATH", "/usr/bin:/bin");
    command.env_remove("BRAINPRINT_ADOPTION_TELEMETRY_PATH");
    command
}

fn run_bin(mut command: Command, stdin: &str) -> (i32, String, String) {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn brainprint-agent");
    use std::io::Write as _;
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn a02_invalid_hook_input_fails_open_with_diagnostics_off_stdout() {
    let runtime = TempDir::create("failopen-rt");
    for (client, native, expected_stdout) in [
        ("claude-code", "PreToolUse", ""),
        ("codex-cli", "PreToolUse", ""),
        ("gemini-cli", "BeforeTool", "{}"),
    ] {
        for garbage in ["not json", "{\"no_session\": true}", ""] {
            let mut command = agent_bin();
            command.env("XDG_RUNTIME_DIR", &runtime.0).args([
                "bridge", "--client", client, "--event", native, "--mode", "guard",
            ]);
            let (code, stdout, stderr) = run_bin(command, garbage);
            assert_eq!(code, 0, "{client}: fail open, never block");
            assert_eq!(
                stdout, expected_stdout,
                "{client}: protocol stdout is the no-op response only"
            );
            assert!(
                stderr.contains("allowing native fallback"),
                "{client}: diagnostic on stderr: {stderr}"
            );
        }
    }
    // Unknown native event, and the normalized path.
    let mut command = agent_bin();
    command.env("XDG_RUNTIME_DIR", &runtime.0).args([
        "bridge",
        "--client",
        "claude-code",
        "--event",
        "NoSuchEvent",
        "--mode",
        "guard",
    ]);
    let (code, stdout, _) = run_bin(command, "{\"session_id\":\"s\"}");
    assert_eq!((code, stdout.as_str()), (0, ""));
    let mut command = agent_bin();
    command
        .env("XDG_RUNTIME_DIR", &runtime.0)
        .args(["event", "--kind", "PRE_TOOL", "--mode", "guard"]);
    let (code, stdout, _) = run_bin(command, "{broken");
    assert_eq!(code, 0);
    let decision: Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(decision["decision"], "allow");
    assert_eq!(decision["fallback_reason"], "BRIDGE_ERROR");
}

#[test]
fn a03_no_engine_daemon_or_mcp_dependency() {
    let manifest = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("manifest");
    let dependencies = manifest
        .split("[dependencies]")
        .nth(1)
        .and_then(|rest| rest.split("\n[").next())
        .expect("dependencies table");
    let names: Vec<&str> = dependencies
        .lines()
        .filter_map(|line| line.split(['=', '.']).next())
        .map(str::trim)
        .filter(|name| !name.is_empty() && !name.starts_with('#'))
        .collect();
    assert_eq!(
        names,
        ["brainprint-core", "clap", "serde", "serde_json", "tokio"]
    );
    assert!(
        !manifest.contains("[dev-dependencies]"),
        "no test-only back door either"
    );
}

fn agent_sources() -> Vec<(String, String)> {
    let mut files = Vec::new();
    let mut stack = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir).expect("src dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let name = path
                    .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                    .expect("rel")
                    .to_string_lossy()
                    .into_owned();
                files.push((name, fs::read_to_string(&path).expect("read")));
            }
        }
    }
    files
}

#[test]
fn a04_a50_zero_direct_db_or_project_source_access() {
    for (name, text) in agent_sources() {
        for forbidden in [
            "rusqlite",
            "index.db",
            "workspace.db",
            "project.db",
            "global.db",
            "brainprint_engine",
            "brainprint_daemon",
            "brainprint_mcp",
        ] {
            assert!(
                !text.contains(forbidden),
                "{name} must not reference {forbidden}"
            );
        }
        // The only file-content reads are the adapter's own state file
        // (state.rs) and the hook payload on stdin (main.rs).
        let reads = text.matches("fs::read(").count()
            + text.matches("File::open").count()
            + text.matches("read_to_string(").count();
        let allowed = match name.as_str() {
            "src/state.rs" => 1,
            "src/main.rs" => 1,
            _ => 0,
        };
        assert_eq!(reads, allowed, "{name}: unexpected content read");
        // No semantic backend / parser / process start except the client
        // `--version` probe.
        let spawns = text.matches("Command::new").count();
        assert_eq!(
            spawns,
            usize::from(name == "src/main.rs"),
            "{name}: unexpected process spawn"
        );
    }
}

#[test]
fn a05_state_is_bounded_ttl_limited_and_body_free() {
    let fixture = with_delivered_source("bounded", Mode::Guard);
    // Body-free: the delivered source body never reaches state.
    let store = fixture.store("claude-code");
    for entry in fs::read_dir(fixture.runtime.0.join("adoption/claude-code")).expect("state dir") {
        let text = fs::read_to_string(entry.expect("entry").path()).expect("state");
        assert!(
            !text.contains(SECRET_SOURCE_MARKER),
            "no source body in state"
        );
        assert!(!text.contains("payload"), "no MCP payload in state");
    }

    // Record-count cap.
    for index in 0..(MAX_RECORDS_PER_CLIENT + 44) {
        let mut record = store.load(&format!("bulk-{index}"));
        store
            .save(&format!("bulk-{index}"), &mut record)
            .expect("save");
    }
    assert!(
        store.record_count() <= MAX_RECORDS_PER_CLIENT,
        "count {}",
        store.record_count()
    );

    // Per-record byte cap under a flood of deliveries.
    for line in 0..400 {
        fixture.deliver(
            "flood",
            BrainprintTool::Inspect,
            json!({}),
            fixture.source_envelope("src/app.rs", line, line + 2),
            Mode::Guard,
        );
    }
    let max = store.max_record_bytes();
    assert!(max as usize <= MAX_RECORD_BYTES, "max record bytes {max}");
    println!(
        "MEASURE state_max_record_bytes={max} records={}",
        store.record_count()
    );

    // The oldest records were evicted by the count cap.
    assert!(store.load("bulk-0").sources.is_empty() && !store.exists("bulk-0"));

    // TTL: a >24h-old record is treated as absent and removed.
    fixture.deliver(
        "ttl",
        BrainprintTool::Inspect,
        json!({}),
        fixture.source_envelope("src/app.rs", 0, 3),
        Mode::Guard,
    );
    assert_eq!(store.load("ttl").sources.len(), 1);
    let path = fixture.runtime.0.join("adoption/claude-code").join(format!(
        "{}.json",
        brainprint_agent::util::fingerprint(["ttl"])
    ));
    let file = fs::File::options()
        .write(true)
        .open(&path)
        .expect("open state");
    file.set_modified(SystemTime::now() - Duration::from_secs(25 * 60 * 60))
        .expect("age it");
    drop(file);
    let reloaded = store.load("ttl");
    assert!(reloaded.sources.is_empty(), "expired record is fresh");
    assert!(!path.exists(), "expired record is removed");
}

// ============================================================== Modes

#[test]
fn a06_observe_never_suppresses() {
    let fixture = with_delivered_source("observe", Mode::Observe);
    assert_eq!(
        fixture.probe.index.borrow().probes,
        0,
        "observe records nothing it will not use"
    );
    // Even with a proven delivery recorded under another mode.
    fixture.deliver(
        "s1",
        BrainprintTool::Inspect,
        json!({}),
        fixture.source_envelope("src/app.rs", 0, 3),
        Mode::Guard,
    );
    fixture.probe.index.borrow_mut().probes = 0;
    let outcome = fixture.pre("s1", read(fixture.abs("src/app.rs"), (0, 3)), Mode::Observe);
    assert_eq!(kind(&outcome), DecisionKind::Allow);
    assert_eq!(outcome.decision.message, None);
    assert_eq!(
        fixture.probe.index.borrow().probes,
        0,
        "observe spends no probe on decisions"
    );
}

#[test]
fn a07_prefer_never_suppresses_but_says_it_is_redundant() {
    let fixture = with_delivered_source("prefer", Mode::Prefer);
    let outcome = fixture.pre("s1", read(fixture.abs("src/app.rs"), (0, 3)), Mode::Prefer);
    assert_eq!(kind(&outcome), DecisionKind::Advise);
    assert!(outcome.exact_substitute_proven);
    assert!(
        outcome
            .decision
            .message
            .as_deref()
            .is_some_and(|message| message.contains("redundant"))
    );
}

#[test]
fn a08_a12_guard_suppresses_exact_delivered_source_repeat() {
    let fixture = with_delivered_source("guard", Mode::Guard);
    for lines in [(0, 3), (1, 2)] {
        let outcome = fixture.pre("s1", read(fixture.abs("src/app.rs"), lines), Mode::Guard);
        assert_eq!(kind(&outcome), DecisionKind::SuppressRedirect, "{lines:?}");
        assert!(outcome.exact_substitute_proven);
        let message = outcome.decision.message.expect("redirect reason");
        assert!(message.contains("bypass-once --session s1"));
        assert!(
            !message.contains(SECRET_SOURCE_MARKER),
            "the redirect never re-sends source"
        );
    }
    // Relative path and the Claude-wire rendering.
    let outcome = fixture.pre("s1", read("src/app.rs".into(), (0, 2)), Mode::Guard);
    assert_eq!(kind(&outcome), DecisionKind::SuppressRedirect);
    let rendered = ClientId::ClaudeCode.render("PreToolUse", &outcome.decision);
    let wire: Value = serde_json::from_str(&rendered.stdout).expect("claude wire");
    assert_eq!(wire["hookSpecificOutput"]["permissionDecision"], "deny");
}

#[test]
fn a09_every_gap_or_uncertain_state_allows_fallback() {
    let fixture = with_delivered_source("gaps", Mode::Guard);
    let attempt = || fixture.pre("s1", read(fixture.abs("src/app.rs"), (0, 3)), Mode::Guard);
    for (fail, expected) in [
        (
            FallbackReason::NotInitialized,
            FallbackReason::NotInitialized,
        ),
        (
            FallbackReason::DaemonUnavailable,
            FallbackReason::DaemonUnavailable,
        ),
        (FallbackReason::Ambiguous, FallbackReason::Ambiguous),
    ] {
        fixture.probe.index.borrow_mut().fail = Some(fail);
        let outcome = attempt();
        assert_eq!(
            (kind(&outcome), outcome.decision.fallback),
            (DecisionKind::Allow, Some(expected))
        );
        fixture.probe.index.borrow_mut().fail = None;
    }
    fixture.probe.index.borrow_mut().current = false;
    let outcome = attempt();
    assert_eq!(
        (kind(&outcome), outcome.decision.fallback),
        (DecisionKind::Allow, Some(FallbackReason::StaleOrNotCurrent))
    );
    fixture.probe.index.borrow_mut().current = true;

    // Partial / stale / truncated / ambiguous *deliveries* never become a
    // suppression basis in the first place.
    let fresh = Fixture::new("gap-deliveries");
    let mut partial = projected(vec![]);
    partial.more_available = true;
    for (label, result) in [
        (
            "truncated-listing",
            QueryResultWire::Find(FindResultWire::Files(FileListingWire {
                entries: vec![fresh.resource_wire("src/app.rs")],
                truncated: true,
                currentness: CurrentnessWire::Current,
                source: ResultSourceWire::StructuralIndex,
            })),
        ),
        (
            "stale-listing",
            QueryResultWire::Find(FindResultWire::Files(FileListingWire {
                entries: vec![fresh.resource_wire("src/app.rs")],
                truncated: false,
                currentness: CurrentnessWire::NotCurrent(NotCurrentReasonWire::ResourceIndexDirty),
                source: ResultSourceWire::StructuralIndex,
            })),
        ),
        (
            "partial-projection",
            QueryResultWire::Inspect(partial.clone()),
        ),
    ] {
        fresh.deliver(
            label,
            BrainprintTool::Find,
            json!({"mode": "files", "recursive": true}),
            envelope("brainprint.find", "files", &result),
            Mode::Guard,
        );
        let outcome = fresh.pre(
            label,
            NativeAction::FileDiscovery { scope: None },
            Mode::Guard,
        );
        assert_ne!(kind(&outcome), DecisionKind::SuppressRedirect, "{label}");
    }
    for status in [
        QueryStatusWire::Truncated,
        QueryStatusWire::Ambiguous,
        QueryStatusWire::Unsupported,
        QueryStatusWire::Refreshing,
    ] {
        fresh.deliver(
            "text-gap",
            BrainprintTool::Find,
            json!({"mode": "text", "pattern": "budget", "regex": false}),
            fresh.text_envelope(&["src/util/mod.rs"], status),
            Mode::Guard,
        );
        let outcome = fresh.pre(
            "text-gap",
            NativeAction::TextSearch {
                pattern: SearchPattern::Literal("budget".into()),
                case_insensitive: false,
                scope: None,
            },
            Mode::Guard,
        );
        assert_eq!(kind(&outcome), DecisionKind::Allow, "{status:?}");
    }
    // A Brainprint error / transport error envelope is a gap too.
    fresh.deliver("err", BrainprintTool::Find, json!({"mode": "files", "recursive": true}), json!({"tool": "brainprint.find", "mode": "files", "outcome": "brainprint_error", "payload": {"code": "NotInitialized"}}), Mode::Guard);
    assert_eq!(
        kind(&fresh.pre(
            "err",
            NativeAction::FileDiscovery { scope: None },
            Mode::Guard
        )),
        DecisionKind::Allow
    );
}

#[test]
fn a10_bypass_once_allows_exactly_one_native_action() {
    let fixture = with_delivered_source("bypass", Mode::Guard);
    let store = fixture.store("claude-code");
    let mut record = store.load("s1");
    record.bypass = true;
    store.save("s1", &mut record).expect("arm");
    let first = fixture.pre("s1", read(fixture.abs("src/app.rs"), (0, 3)), Mode::Guard);
    assert_eq!(
        (kind(&first), first.decision.fallback),
        (DecisionKind::Allow, Some(FallbackReason::UserBypass))
    );
    let second = fixture.pre("s1", read(fixture.abs("src/app.rs"), (0, 3)), Mode::Guard);
    assert_eq!(
        kind(&second),
        DecisionKind::SuppressRedirect,
        "bypass is one-shot"
    );
    assert!(!store.load("s1").bypass);
}

#[test]
fn a10_bypass_once_command_arms_the_session() {
    let runtime = TempDir::create("bypass-cmd");
    let mut command = agent_bin();
    command
        .env("XDG_RUNTIME_DIR", &runtime.0)
        .args(["bypass-once", "--session", "sess-x"]);
    let (code, _, _) = run_bin(command, "");
    assert_ne!(code, 0, "unknown session without --client is refused");
    let mut command = agent_bin();
    command.env("XDG_RUNTIME_DIR", &runtime.0).args([
        "bypass-once",
        "--session",
        "sess-x",
        "--client",
        "claude-code",
    ]);
    let (code, stdout, _) = run_bin(command, "");
    assert_eq!(code, 0);
    assert!(stdout.contains("claude-code"));
    assert!(
        StateStore::new(&runtime.0.join("brainprint"), "claude-code")
            .load("sess-x")
            .bypass
    );
}

#[test]
fn a11_daemon_failure_allows_fallback_fast() {
    let fixture = with_delivered_source("daemon-down", Mode::Guard);
    let bogus = TempDir::create("no-daemon");
    let mut probe = IpcProbe::new(Some(EndpointPaths::from_runtime_root(bogus.0.clone())));
    let event = fixture.event(
        EventKind::PreTool,
        "s1",
        Some(read(fixture.abs("src/app.rs"), (0, 3))),
    );
    let started = std::time::Instant::now();
    let outcome = gateway::handle(
        &event,
        Mode::Guard,
        &fixture.store("claude-code"),
        &mut probe,
    );
    assert_eq!(
        (kind(&outcome), outcome.decision.fallback),
        (DecisionKind::Allow, Some(FallbackReason::DaemonUnavailable))
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

// ========================================================= Redundancy

#[test]
fn a13_larger_or_different_range_is_allowed() {
    let fixture = with_delivered_source("ranges", Mode::Guard);
    for (lines, reason) in [
        (
            Some((0, 4)),
            Some(FallbackReason::RequestExceedsDeliveredRange),
        ),
        (
            Some((2, 5)),
            Some(FallbackReason::RequestExceedsDeliveredRange),
        ),
        (None, Some(FallbackReason::RequestExceedsDeliveredRange)),
    ] {
        let outcome = fixture.pre(
            "s1",
            NativeAction::SourceRead {
                path: fixture.abs("src/app.rs"),
                lines,
            },
            Mode::Guard,
        );
        assert_eq!(kind(&outcome), DecisionKind::Allow, "{lines:?}");
        assert_eq!(outcome.decision.fallback, reason);
    }
    // A file never delivered: no substitute, allowed.
    let other = fixture.pre("s1", read(fixture.abs("src/lib.rs"), (0, 1)), Mode::Guard);
    assert_eq!(kind(&other), DecisionKind::Allow);
    assert!(!other.exact_substitute_proven);
    // Another session never saw the delivery.
    assert_ne!(
        kind(&fixture.pre("s2", read(fixture.abs("src/app.rs"), (0, 3)), Mode::Guard)),
        DecisionKind::SuppressRedirect
    );
}

#[test]
fn a14_changed_or_unproven_revision_is_allowed() {
    let fixture = with_delivered_source("revision", Mode::Guard);
    fixture.revise("src/app.rs");
    let outcome = fixture.pre("s1", read(fixture.abs("src/app.rs"), (0, 3)), Mode::Guard);
    assert_eq!(
        (kind(&outcome), outcome.decision.fallback),
        (DecisionKind::Allow, Some(FallbackReason::StaleOrNotCurrent))
    );

    // An observed native write forgets the delivery immediately, even
    // before the index notices.
    let fixture = with_delivered_source("write-invalidates", Mode::Guard);
    fixture.pre(
        "s1",
        NativeAction::Write {
            path: fixture.abs("src/app.rs"),
        },
        Mode::Guard,
    );
    assert_eq!(
        kind(&fixture.pre("s1", read(fixture.abs("src/app.rs"), (0, 3)), Mode::Guard)),
        DecisionKind::Allow
    );
    // So does any opaque command (unknown side effects).
    let fixture = with_delivered_source("opaque-invalidates", Mode::Guard);
    fixture.pre("s1", NativeAction::Opaque, Mode::Guard);
    assert_eq!(
        kind(&fixture.pre("s1", read(fixture.abs("src/app.rs"), (0, 3)), Mode::Guard)),
        DecisionKind::Allow
    );
    // A delivery whose revision the index does not confirm is never recorded.
    let fixture = Fixture::new("unconfirmed");
    let envelope = fixture.source_envelope("src/app.rs", 0, 3);
    fixture.revise("src/app.rs");
    fixture.deliver(
        "s1",
        BrainprintTool::Inspect,
        json!({}),
        envelope,
        Mode::Guard,
    );
    assert!(fixture.store("claude-code").load("s1").sources.is_empty());
}

#[test]
fn a15_equivalent_completed_file_discovery_is_suppressed() {
    let fixture = Fixture::new("discovery");
    fixture.deliver(
        "s1",
        BrainprintTool::Find,
        json!({"mode": "files", "directory": "src", "recursive": true}),
        fixture.listing_envelope(Some("src")),
        Mode::Guard,
    );
    fixture.deliver(
        "s1",
        BrainprintTool::Find,
        json!({"mode": "files", "recursive": true}),
        fixture.listing_envelope(None),
        Mode::Guard,
    );
    for action in [
        NativeAction::FileDiscovery {
            scope: Some(fixture.abs("src")),
        },
        NativeAction::FileDiscovery {
            scope: Some("src".into()),
        },
        NativeAction::FileDiscovery { scope: None },
        brainprint_agent::shell::classify("rg --files src"),
        brainprint_agent::shell::classify("find src -type f"),
        brainprint_agent::shell::classify("find . -type f"),
    ] {
        let outcome = fixture.pre("s1", action.clone(), Mode::Guard);
        assert_eq!(kind(&outcome), DecisionKind::SuppressRedirect, "{action:?}");
    }
}

#[test]
fn a16_non_equivalent_file_discovery_is_allowed() {
    let fixture = Fixture::new("discovery-neq");
    fixture.deliver(
        "s1",
        BrainprintTool::Find,
        json!({"mode": "files", "directory": "src", "recursive": true}),
        fixture.listing_envelope(Some("src")),
        Mode::Guard,
    );
    // Different scope, filtered forms, non-recursive forms.
    let claude = |input: Value| {
        let payload = json!({"session_id": "s1", "cwd": fixture.root, "tool_name": "Glob", "tool_input": input});
        ClientId::ClaudeCode
            .normalize("PreToolUse", &payload, None)
            .expect("normalize")
            .action
            .expect("action")
    };
    for action in [
        NativeAction::FileDiscovery {
            scope: Some("docs".into()),
        },
        NativeAction::FileDiscovery {
            scope: Some("src/util".into()),
        },
        claude(json!({"pattern": "**/*.rs", "path": fixture.abs("src")})),
        brainprint_agent::shell::classify("ls src"),
        brainprint_agent::shell::classify("find src -name '*.rs'"),
        brainprint_agent::shell::classify("rg --files --hidden src"),
    ] {
        assert_eq!(
            kind(&fixture.pre("s1", action.clone(), Mode::Guard)),
            DecisionKind::Allow,
            "{action:?}"
        );
    }
    // A listing filtered by role is never a suppression basis.
    fixture.deliver(
        "s2",
        BrainprintTool::Find,
        json!({"mode": "files", "recursive": true, "role": "Source"}),
        fixture.listing_envelope(None),
        Mode::Guard,
    );
    assert_eq!(
        kind(&fixture.pre(
            "s2",
            NativeAction::FileDiscovery { scope: None },
            Mode::Guard
        )),
        DecisionKind::Allow
    );
    // A file added since delivery changes the listing digest.
    fixture
        .probe
        .index
        .borrow_mut()
        .entries
        .push(ListedResource {
            id: ResourceId::generate().to_string(),
            path_rel: "src/new.rs".into(),
            revision: "r".into(),
            is_file: true,
        });
    let outcome = fixture.pre(
        "s1",
        NativeAction::FileDiscovery {
            scope: Some("src".into()),
        },
        Mode::Guard,
    );
    assert_eq!(
        (kind(&outcome), outcome.decision.fallback),
        (DecisionKind::Allow, Some(FallbackReason::StaleOrNotCurrent))
    );
}

#[test]
fn a17_equivalent_completed_text_search_is_suppressed() {
    let fixture = Fixture::new("search");
    fixture.deliver(
        "s1",
        BrainprintTool::Find,
        json!({"mode": "text", "pattern": "budget", "regex": false, "path_prefix": "src/"}),
        fixture.text_envelope(&["src/util/mod.rs"], QueryStatusWire::Found),
        Mode::Guard,
    );
    fixture.deliver(
        "s1",
        BrainprintTool::Find,
        json!({"mode": "text", "pattern": "fn \\w+", "regex": true}),
        fixture.text_envelope(&["src/app.rs"], QueryStatusWire::Found),
        Mode::Guard,
    );
    let grep = |input: Value| {
        let payload = json!({"session_id": "s1", "cwd": fixture.root, "tool_name": "Grep", "tool_input": input});
        ClientId::ClaudeCode
            .normalize("PreToolUse", &payload, None)
            .expect("normalize")
            .action
            .expect("action")
    };
    for action in [
        brainprint_agent::shell::classify("rg -l -F budget src"),
        brainprint_agent::shell::classify("rg -c --fixed-strings budget src"),
        grep(json!({"pattern": "fn \\w+"})),
        grep(json!({"pattern": "fn \\w+", "output_mode": "count", "path": fixture.root})),
    ] {
        assert_eq!(
            kind(&fixture.pre("s1", action.clone(), Mode::Guard)),
            DecisionKind::SuppressRedirect,
            "{action:?}"
        );
    }
    // Different case semantics / pattern kind / scope: allowed.
    for action in [
        brainprint_agent::shell::classify("rg -l -F -i budget src"),
        brainprint_agent::shell::classify("rg -l budget src"),
        brainprint_agent::shell::classify("rg -l -F budget"),
        brainprint_agent::shell::classify("rg -l -F budget src/util"),
    ] {
        assert_eq!(
            kind(&fixture.pre("s1", action.clone(), Mode::Guard)),
            DecisionKind::Allow,
            "{action:?}"
        );
    }
    // NotFound without an explicit workspace_path cannot be tied to a
    // Workspace: never recorded.
    fixture.deliver(
        "s3",
        BrainprintTool::Find,
        json!({"mode": "text", "pattern": "zzz", "regex": false}),
        fixture.text_envelope(&[], QueryStatusWire::NotFound),
        Mode::Guard,
    );
    assert!(fixture.store("claude-code").load("s3").searches.is_empty());
    fixture.deliver(
        "s4",
        BrainprintTool::Find,
        json!({"mode": "text", "pattern": "zzz", "regex": false, "workspace_path": fixture.root}),
        fixture.text_envelope(&[], QueryStatusWire::NotFound),
        Mode::Guard,
    );
    assert_eq!(
        kind(&fixture.pre(
            "s4",
            brainprint_agent::shell::classify("rg -l -F zzz"),
            Mode::Guard
        )),
        DecisionKind::SuppressRedirect
    );
}

#[test]
fn a18_unsupported_search_flags_are_allowed() {
    let fixture = Fixture::new("flags");
    fixture.deliver(
        "s1",
        BrainprintTool::Find,
        json!({"mode": "text", "pattern": "budget", "regex": false}),
        fixture.text_envelope(&["src/util/mod.rs"], QueryStatusWire::Found),
        Mode::Guard,
    );
    let grep = |input: Value| {
        let payload = json!({"session_id": "s1", "cwd": fixture.root, "tool_name": "Grep", "tool_input": input});
        ClientId::ClaudeCode
            .normalize("PreToolUse", &payload, None)
            .expect("normalize")
            .action
            .expect("action")
    };
    for (action, reason) in [
        (
            grep(json!({"pattern": "budget", "glob": "*.rs"})),
            FallbackReason::UnsupportedFlags,
        ),
        (
            grep(json!({"pattern": "budget", "-A": 2})),
            FallbackReason::UnsupportedFlags,
        ),
        (
            grep(json!({"pattern": "budget", "output_mode": "content"})),
            FallbackReason::UnprovenEquivalence,
        ),
        (
            brainprint_agent::shell::classify("rg -l -F --hidden budget"),
            FallbackReason::UnsupportedFlags,
        ),
        (
            brainprint_agent::shell::classify("rg -F budget"),
            FallbackReason::UnprovenEquivalence,
        ),
        (
            brainprint_agent::shell::classify("grep -rl budget ."),
            FallbackReason::UnsupportedFlags,
        ),
    ] {
        let outcome = fixture.pre("s1", action.clone(), Mode::Guard);
        assert_eq!(
            (kind(&outcome), outcome.decision.fallback),
            (DecisionKind::Allow, Some(reason)),
            "{action:?}"
        );
    }
}

#[test]
fn a19_complex_or_unproven_command_semantics_are_allowed() {
    let fixture = with_delivered_source("complex", Mode::Guard);
    fixture.deliver(
        "s1",
        BrainprintTool::Find,
        json!({"mode": "files", "recursive": true}),
        fixture.listing_envelope(None),
        Mode::Guard,
    );
    for command in [
        "sed -n '1,3p' src/app.rs | cat",
        "cat src/app.rs && echo done",
        "cat $(echo src/app.rs)",
        "rg --files > /tmp/x",
        "find . -type f -newer src/lib.rs",
        "FOO=1 cat src/app.rs",
        "sed -n '1,3p' src/app.rs; ls",
        "python3 -c 'print(open(\"src/app.rs\").read())'",
    ] {
        let outcome = fixture.pre(
            "s1",
            brainprint_agent::shell::classify(command),
            Mode::Guard,
        );
        assert_eq!(kind(&outcome), DecisionKind::Allow, "{command}");
    }
}

// ========================================================= Continuity

fn boundary(
    fixture: &Fixture,
    kind: EventKind,
    session: &str,
    source: Option<ResetSource>,
    inject: bool,
) -> Outcome {
    let mut event = fixture.event(kind, session, None);
    event.reset_source = source;
    event.can_inject_context = inject;
    fixture.run(&event, Mode::Prefer)
}

#[test]
fn a20_a24_bootstrap_once_per_boundary_never_every_turn() {
    let fixture = Fixture::new("continuity");
    // 20: startup once; a duplicate startup event is idempotent.
    assert_eq!(
        boundary(
            &fixture,
            EventKind::SessionStarted,
            "c",
            Some(ResetSource::Startup),
            true
        )
        .decision
        .bootstrap,
        Some(BOOTSTRAP)
    );
    assert_eq!(
        boundary(
            &fixture,
            EventKind::SessionStarted,
            "c",
            Some(ResetSource::Startup),
            true
        )
        .decision
        .bootstrap,
        None
    );
    // 24: fifty tool turns later, no re-injection.
    for _ in 0..50 {
        let outcome = fixture.run(
            &fixture.event(EventKind::PreTool, "c", Some(NativeAction::Inert)),
            Mode::Prefer,
        );
        assert_eq!(outcome.decision.bootstrap, None);
        let outcome = fixture.run(
            &fixture.event(EventKind::PostTool, "c", Some(NativeAction::Inert)),
            Mode::Prefer,
        );
        assert_eq!(outcome.decision.bootstrap, None);
    }
    // 21: resume and clear each bootstrap once.
    for source in [ResetSource::Resume, ResetSource::Clear] {
        assert_eq!(
            boundary(&fixture, EventKind::SessionReset, "c", Some(source), true)
                .decision
                .bootstrap,
            Some(BOOTSTRAP)
        );
        assert_eq!(
            fixture
                .run(
                    &fixture.event(EventKind::PreTool, "c", Some(NativeAction::Inert)),
                    Mode::Prefer
                )
                .decision
                .bootstrap,
            None
        );
    }
    // 22: an advisory-only compaction marker -> exactly one later
    // bootstrap at the next event that can carry it.
    assert_eq!(
        boundary(
            &fixture,
            EventKind::SessionReset,
            "c",
            Some(ResetSource::Compaction),
            false
        )
        .decision
        .bootstrap,
        None
    );
    let mut blind = fixture.event(EventKind::PreTool, "c", Some(NativeAction::Inert));
    blind.can_inject_context = false;
    assert_eq!(
        fixture.run(&blind, Mode::Prefer).decision.bootstrap,
        None,
        "not on a channel that cannot carry it"
    );
    assert_eq!(
        fixture
            .run(
                &fixture.event(EventKind::PostTool, "c", Some(NativeAction::Inert)),
                Mode::Prefer
            )
            .decision
            .bootstrap,
        Some(BOOTSTRAP)
    );
    assert_eq!(
        fixture
            .run(
                &fixture.event(EventKind::PostTool, "c", Some(NativeAction::Inert)),
                Mode::Prefer
            )
            .decision
            .bootstrap,
        None
    );
    // 23: subagent start, once per agent id.
    let mut sub = fixture.event(EventKind::SessionStarted, "c", None);
    sub.reset_source = Some(ResetSource::Subagent);
    sub.agent_id = Some("agent-1".into());
    assert_eq!(
        fixture.run(&sub, Mode::Prefer).decision.bootstrap,
        Some(BOOTSTRAP)
    );
    assert_eq!(fixture.run(&sub, Mode::Prefer).decision.bootstrap, None);
    sub.agent_id = Some("agent-2".into());
    assert_eq!(
        fixture.run(&sub, Mode::Prefer).decision.bootstrap,
        Some(BOOTSTRAP)
    );
    println!("MEASURE bootstrap_bytes={}", BOOTSTRAP.len());
}

#[test]
fn reset_forgets_deliveries_and_subagents_do_not_inherit_them() {
    let fixture = with_delivered_source("reset-forgets", Mode::Guard);
    let attempt = |event: &IntegrationEvent| kind(&fixture.run(event, Mode::Guard));
    let native = fixture.event(
        EventKind::PreTool,
        "s1",
        Some(read(fixture.abs("src/app.rs"), (0, 3))),
    );
    assert_eq!(attempt(&native), DecisionKind::SuppressRedirect);
    let mut subagent_read = native.clone();
    subagent_read.agent_id = Some("worker".into());
    assert_ne!(
        attempt(&subagent_read),
        DecisionKind::SuppressRedirect,
        "a subagent's context never saw the parent's delivery"
    );
    boundary(
        &fixture,
        EventKind::SessionReset,
        "s1",
        Some(ResetSource::Clear),
        true,
    );
    assert_ne!(
        attempt(&native),
        DecisionKind::SuppressRedirect,
        "cleared context no longer holds the delivery"
    );
}

// ========================================================== Telemetry

fn route_after(fixture: &Fixture, session: &str, steps: &[&str]) -> Option<&'static str> {
    let mut route = None;
    for step in steps {
        let outcome = match *step {
            "bp-ok" => fixture.deliver(
                session,
                BrainprintTool::Find,
                json!({"mode": "files", "recursive": true}),
                fixture.listing_envelope(None),
                Mode::Prefer,
            ),
            "bp-gap" => fixture.deliver(
                session,
                BrainprintTool::Find,
                json!({"mode": "files", "recursive": true}),
                envelope(
                    "brainprint.find",
                    "files",
                    &QueryResultWire::Find(FindResultWire::Files(FileListingWire {
                        entries: vec![],
                        truncated: true,
                        currentness: CurrentnessWire::Current,
                        source: ResultSourceWire::StructuralIndex,
                    })),
                ),
                Mode::Prefer,
            ),
            "native" => fixture.pre(
                session,
                brainprint_agent::shell::classify("rg -l -F budget"),
                Mode::Prefer,
            ),
            "edit" => fixture.pre(
                session,
                NativeAction::Write {
                    path: fixture.abs("src/app.rs"),
                },
                Mode::Prefer,
            ),
            "build" => fixture.pre(
                session,
                brainprint_agent::shell::classify("cargo build"),
                Mode::Prefer,
            ),
            other => panic!("unknown step {other}"),
        };
        route = outcome.route;
    }
    route
}

#[test]
fn a25_all_four_route_classifications() {
    let fixture = Fixture::new("routes");
    assert_eq!(
        route_after(&fixture, "r1", &["bp-ok", "native"]),
        Some("brainprint_first")
    );
    assert_eq!(
        route_after(&fixture, "r2", &["native", "bp-ok"]),
        Some("native_first_then_brainprint")
    );
    assert_eq!(
        route_after(&fixture, "r3", &["native", "native"]),
        Some("native_only")
    );
    assert_eq!(
        route_after(&fixture, "r4", &["bp-gap", "native"]),
        Some("fallback_required")
    );
    // Edits/builds are not project-understanding exploration.
    assert_eq!(route_after(&fixture, "r5", &["edit", "build"]), None);
    assert_eq!(
        route_after(&fixture, "r6", &["edit", "build", "bp-ok"]),
        Some("brainprint_first")
    );
}

#[test]
fn a26_advice_suppression_and_fallback_metrics_are_factual() {
    let fixture = with_delivered_source("metrics", Mode::Guard);
    let suppressed = fixture.pre("s1", read(fixture.abs("src/app.rs"), (0, 3)), Mode::Guard);
    assert!(
        suppressed.exact_substitute_proven
            && suppressed.decision.kind == DecisionKind::SuppressRedirect
    );
    let fell_back = fixture.pre("s1", read(fixture.abs("src/app.rs"), (0, 9)), Mode::Guard);
    assert_eq!(
        (
            fell_back.exact_substitute_proven,
            fell_back.decision.fallback
        ),
        (false, Some(FallbackReason::RequestExceedsDeliveredRange))
    );
    // First-route advice before any delivery: once per class per epoch.
    let fresh = Fixture::new("advice");
    let first = fresh.pre(
        "a",
        brainprint_agent::shell::classify("rg -l -F budget"),
        Mode::Prefer,
    );
    assert_eq!(first.decision.kind, DecisionKind::Advise);
    assert_eq!(first.suggested, Some("brainprint.find (mode: text)"));
    assert!(
        first
            .decision
            .message
            .as_deref()
            .is_some_and(|text| text.contains("brainprint.find (mode: text)"))
    );
    let again = fresh.pre(
        "a",
        brainprint_agent::shell::classify("rg -l -F other"),
        Mode::Prefer,
    );
    assert_eq!(
        again.decision.kind,
        DecisionKind::Allow,
        "advice is not repeated every turn"
    );
    // No advice when the Workspace is not initialized.
    fresh.probe.index.borrow_mut().fail = Some(FallbackReason::NotInitialized);
    let uninit = fresh.pre(
        "b",
        brainprint_agent::shell::classify("rg -l -F budget"),
        Mode::Prefer,
    );
    assert_eq!(
        (uninit.decision.kind, uninit.decision.fallback),
        (DecisionKind::Allow, Some(FallbackReason::NotInitialized))
    );
}

fn claude_payload(
    fixture: &Fixture,
    session: &str,
    tool: &str,
    input: Value,
    response: Option<Value>,
) -> String {
    let mut payload = json!({"session_id": session, "cwd": fixture.root, "tool_name": tool, "tool_input": input, "transcript_path": "/secret/transcript.jsonl"});
    if let Some(response) = response {
        payload["tool_response"] = response;
    }
    payload.to_string()
}

#[test]
fn a27_a28_telemetry_off_by_default_and_body_free_when_on() {
    let fixture = Fixture::new("telemetry");
    let runtime_env = fixture.runtime.0.clone();
    let sink = fixture.runtime.0.join("adoption.jsonl");
    let bridge = |event: &str, stdin: String, telemetry: bool| {
        let mut command = agent_bin();
        command.env("XDG_RUNTIME_DIR", &runtime_env).args([
            "bridge",
            "--client",
            "claude-code",
            "--event",
            event,
            "--mode",
            "prefer",
            "--client-version",
            "2.1.283",
        ]);
        if telemetry {
            command.env("BRAINPRINT_ADOPTION_TELEMETRY_PATH", &sink);
        }
        run_bin(command, &stdin)
    };
    let prompt_like = claude_payload(
        &fixture,
        "t1",
        "Read",
        json!({"file_path": fixture.abs("src/app.rs")}),
        Some(json!({"content": SECRET_SOURCE_MARKER})),
    );
    bridge("PostToolUse", prompt_like.clone(), false);
    assert!(!sink.exists(), "27: disabled unless the env var is set");

    bridge(
        "SessionStart",
        json!({"session_id": "t1", "cwd": fixture.root, "source": "startup"}).to_string(),
        true,
    );
    bridge(
        "PreToolUse",
        claude_payload(
            &fixture,
            "t1",
            "Read",
            json!({"file_path": fixture.abs("src/app.rs")}),
            None,
        ),
        true,
    );
    bridge("PostToolUse", prompt_like, true);
    let text = fs::read_to_string(&sink).expect("telemetry written");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("jsonl"))
        .collect();
    assert_eq!(lines.len(), 3);
    for line in &lines {
        for field in [
            "timestamp_unix_ms",
            "client",
            "client_version",
            "session",
            "mode",
            "event",
            "route",
            "exact_substitute_proven",
            "decision",
            "fallback_reason",
            "latency_us",
            "daemon_probe_count",
        ] {
            assert!(line.get(field).is_some(), "missing {field}");
        }
        assert_eq!(line["client_version"], "2.1.283");
    }
    assert_eq!(lines[1]["native_attempt"], "source read");
    assert_eq!(lines[1]["route"], "native_only");
    // 28: no prompt/source/transcript/tool-result body, no raw session id.
    for forbidden in [
        SECRET_SOURCE_MARKER,
        "/secret/transcript",
        "\"t1\"",
        fixture.root.as_str(),
    ] {
        assert!(!text.contains(forbidden), "telemetry leaked {forbidden}");
    }
    println!(
        "MEASURE telemetry_event_bytes_max={}",
        text.lines().map(str::len).max().unwrap_or(0) + 1
    );
}

// ============================================== Capability / gateway

#[test]
fn a46_normalized_event_path_is_independent_of_client_formats() {
    let runtime = TempDir::create("normalized");
    let mut command = agent_bin();
    command.env("XDG_RUNTIME_DIR", &runtime.0).args([
        "event",
        "--kind",
        "SESSION_STARTED",
        "--mode",
        "prefer",
    ]);
    let (code, stdout, _) = run_bin(
        command,
        &json!({"client_id": "any-agent", "session_id": "n1", "can_inject_context": true})
            .to_string(),
    );
    assert_eq!(code, 0);
    let first: Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(first["bootstrap"], BOOTSTRAP);
    let mut command = agent_bin();
    command
        .env("XDG_RUNTIME_DIR", &runtime.0)
        .args(["event", "--kind", "PRE_TOOL", "--mode", "guard"]);
    let (_, stdout, _) = run_bin(command, &json!({"client_id": "any-agent", "session_id": "n1", "action": {"type": "unproven_exploration", "class": "TEXT_SEARCH", "reason": "UNSUPPORTED_FLAGS"}}).to_string());
    let decision: Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(decision["decision"], "allow");
    assert_eq!(decision["fallback_reason"], "UNSUPPORTED_FLAGS");
}

#[test]
fn a47_a49_same_normalized_event_same_decision_for_any_client() {
    let fixture = with_delivered_source("brand-free", Mode::Guard);
    let native_read = |client: &str| {
        let mut event = fixture.event(
            EventKind::PreTool,
            "s1",
            Some(brainprint_agent::shell::classify(
                "sed -n '1,3p' src/app.rs",
            )),
        );
        event.client_id = client.to_owned();
        event
    };
    // Deliver the same fact under a fixture client with Claude-equivalent
    // capabilities: no new substitution/freshness logic is involved.
    let mut delivery = fixture.event(
        EventKind::PostTool,
        "s1",
        Some(NativeAction::Brainprint {
            tool: BrainprintTool::Inspect,
            input: json!({}),
            result: Some(fixture.source_envelope("src/app.rs", 0, 3)),
        }),
    );
    delivery.client_id = "fixture-agent".to_owned();
    fixture.run(&delivery, Mode::Guard);
    let decisions: Vec<Decision> = ["claude-code", "fixture-agent"]
        .iter()
        .map(|client| fixture.run(&native_read(client), Mode::Guard).decision)
        .collect();
    assert_eq!(decisions[0], decisions[1]);
    assert_eq!(decisions[0].kind, DecisionKind::SuppressRedirect);
    // The Claude and Codex bridges translate the same shell read into the
    // same normalized action.
    let payload = json!({"session_id": "s1", "cwd": fixture.root, "tool_name": "Bash", "tool_input": {"command": "sed -n '1,3p' src/app.rs"}});
    let claude = ClientId::ClaudeCode
        .normalize("PreToolUse", &payload, None)
        .expect("claude")
        .action;
    let codex = ClientId::CodexCli
        .normalize("PreToolUse", &payload, None)
        .expect("codex")
        .action;
    assert_eq!(claude, codex);
    let gemini = ClientId::GeminiCli
        .normalize("BeforeTool", &json!({"session_id": "s1", "cwd": fixture.root, "tool_name": "run_shell_command", "tool_input": {"command": "sed -n '1,3p' src/app.rs"}}), None)
        .expect("gemini")
        .action;
    assert_eq!(claude, gemini);
}

#[test]
fn a48_capability_absence_degrades_without_disabling_core_queries() {
    let fixture = with_delivered_source("degrade", Mode::Guard);
    // No agent attribution: guard -> advice + UNPROVEN_EQUIVALENCE.
    let mut event = fixture.event(
        EventKind::PreTool,
        "s1",
        Some(read(fixture.abs("src/app.rs"), (0, 3))),
    );
    event.capabilities.agent_scoped_tool_events = false;
    let outcome = fixture.run(&event, Mode::Guard);
    assert_eq!(
        (outcome.decision.kind, outcome.decision.fallback),
        (
            DecisionKind::Advise,
            Some(FallbackReason::UnprovenEquivalence)
        )
    );
    // No post-tool result: nothing is claimed delivered.
    let fresh = Fixture::new("no-result");
    let mut post = fresh.event(
        EventKind::PostTool,
        "s1",
        Some(NativeAction::Brainprint {
            tool: BrainprintTool::Inspect,
            input: json!({}),
            result: Some(fresh.source_envelope("src/app.rs", 0, 3)),
        }),
    );
    post.capabilities = ClientCapabilities {
        post_tool_result: false,
        ..ClientId::ClaudeCode.capabilities()
    };
    fresh.run(&post, Mode::Guard);
    assert!(fresh.store("claude-code").load("s1").sources.is_empty());
    // Gemini (no pre-tool context, no agent scope): its guard never
    // suppresses and its advice rides on the post-tool result.
    let gemini = Fixture::new("gemini-degrade");
    let mut pre = gemini.event(
        EventKind::PreTool,
        "g",
        Some(brainprint_agent::shell::classify("rg -l -F budget")),
    );
    pre.capabilities = ClientId::GeminiCli.capabilities();
    pre.can_inject_context = false;
    assert_eq!(gemini.run(&pre, Mode::Guard).decision.message, None);
    let mut post = pre.clone();
    post.kind = EventKind::PostTool;
    post.can_inject_context = true;
    let advised = gemini.run(&post, Mode::Guard);
    assert_eq!(advised.decision.kind, DecisionKind::Advise);
    let rendered = ClientId::GeminiCli.render("AfterTool", &advised.decision);
    assert!(rendered.stdout.contains("additionalContext"));
}

#[test]
fn a51_context_usage_is_telemetry_only() {
    let fixture = Fixture::new("usage");
    let outcome = fixture.run(
        &fixture.event(EventKind::ContextUsage, "u", None),
        Mode::Guard,
    );
    assert_eq!(outcome.decision, Decision::allow());
    assert!(outcome.observed);
    assert_eq!(fixture.probe.index.borrow().probes, 0);
}

#[test]
fn a52_a53_no_reference_product_routing() {
    for (name, text) in agent_sources() {
        let lower = text.to_lowercase();
        for product in [
            "serena",
            "codegraph",
            "claude-mem",
            "claude_mem",
            "openviking",
            "headroom",
            "\"rtk\"",
            "rtk ",
        ] {
            assert!(
                !lower.contains(product),
                "{name} references reference product {product}"
            );
        }
    }
    // Every spawn is the configured client's own `--version`.
    let main = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs"))
        .expect("main");
    assert!(main.contains("Command::new(client.binary())"));
}

#[test]
fn config_fragments_are_deterministic_and_never_written() {
    let runtime = TempDir::create("config");
    for client in ["claude-code", "gemini-cli", "codex-cli"] {
        let run = || {
            let mut command = agent_bin();
            command
                .env("XDG_RUNTIME_DIR", &runtime.0)
                .env("HOME", &runtime.0)
                .args(["config", "--client", client, "--mode", "prefer"]);
            run_bin(command, "")
        };
        let (code, first, _) = run();
        let (_, second, _) = run();
        assert_eq!(code, 0);
        assert_eq!(first, second, "{client}: deterministic");
        let fragment: Value = serde_json::from_str(&first).expect("json fragment");
        assert!(fragment["hooks"].is_object());
        assert!(
            first.contains("--mode prefer"),
            "no client is silently switched to guard"
        );
    }
    assert!(
        fs::read_dir(&runtime.0).expect("dir").next().is_none(),
        "config wrote nothing"
    );
    let _ = ActionClass::SourceRead;
}

fn repo_file(rel: &str) -> String {
    fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(rel),
    )
    .expect("repo file")
}

#[test]
fn checked_in_client_fragments_match_the_generator() {
    for (client, file) in [
        (
            "claude-code",
            "integrations/brainprint/clients/claude-code/settings.hooks.json",
        ),
        (
            "gemini-cli",
            "integrations/brainprint/clients/gemini-cli/settings.hooks.json",
        ),
        (
            "codex-cli",
            "integrations/brainprint/clients/codex-cli/hooks.json",
        ),
    ] {
        let mut command = agent_bin();
        command.args(["config", "--client", client, "--mode", "prefer"]);
        let (_, generated, _) = run_bin(command, "");
        assert_eq!(
            repo_file(file),
            generated,
            "{file} is the canonical generated fragment"
        );
    }
}

#[test]
fn a34_a36_claude_md_is_a_thin_brainprint_first_pointer() {
    let text = repo_file("CLAUDE.md");
    // 34: Brainprint first, thin (no Skill/schema copy).
    assert!(text.contains("Brainprint를 먼저"));
    for tool in [
        "brainprint.find",
        "brainprint.inspect",
        "brainprint.relations",
        "brainprint.context",
    ] {
        assert!(text.contains(tool), "{tool}");
    }
    assert!(
        text.len() < 2_000,
        "thin pointer, not a Skill copy ({} bytes)",
        text.len()
    );
    // 35: no mandatory CodeGraph-first instruction.
    for forbidden in ["CodeGraph 우선", "CodeGraph를 먼저", "codegraph_explore"] {
        assert!(!text.contains(forbidden), "{forbidden}");
    }
    // 36: fallback remains documented.
    for gap in [
        "partial",
        "stale",
        "unsupported",
        "ambiguous",
        "truncated",
        "원본 검증",
    ] {
        assert!(text.contains(gap), "{gap}");
    }
}
