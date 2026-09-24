//! The shared fleet: many clients, many contexts, many backends
//! (#19 task 14).
//!
//! Tasks 9-13 proved what each language can be asked. This file proves
//! that the machinery underneath them stays correct when several
//! clients, several [`AnalysisContext`]s, several backend families,
//! several worktrees and a resource bound are all in play at once.
//!
//! It has two halves, and task 14 needs both.
//!
//! **Always on.** Everything about publication, freshness, worktrees
//! and runtime-versus-truth runs here with no external tooling at all,
//! against fake hosts and a real `index.db`. These are ordinary tests.
//!
//! **The real fleet.** The heterogeneous topology -- five launchers in
//! one supervisor -- is `#[ignore]`d, because it needs five separate
//! installations, and skips with a printed reason when one is missing.
//!
//! ```sh
//! cargo test -p brainprint-engine --test i4_fleet_acceptance
//! cargo test -p brainprint-engine --test i4_fleet_acceptance -- --ignored
//! ```
//!
//! ## What this file is not
//!
//! It is not another language task. Where a fixture here brushes
//! against a capability tasks 9-13 already recorded as PARTIAL or
//! UNSUPPORTED, it leaves it exactly as recorded. A runtime that is
//! evicted, degraded or absent changes *coverage*, never a capability
//! verdict: those are different axes and this file keeps them apart.
//!
//! It also introduces no Agent. There is no client registry, no session
//! identity and no per-client backend here, because the thing being
//! proved is that a "client" is nothing but an anonymous caller holding
//! a lease.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process,
    sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use brainprint_core::{ResourceId, WorkspaceId};
use brainprint_engine::{
    config::WorkspaceConfig,
    generation::GenerationStore,
    logical_symbol::LogicalIdentity,
    relations::RelationIndex,
    resolution::Support,
    resource::{ResourceLanguage, ResourceStore},
    runtime::{
        CancelToken, HostConcurrency, HostError, HostHealth, RequestOptions, RuntimePolicy,
        RuntimeRequest, RuntimeResponse, RuntimeState, SemanticBackendLauncher, SemanticRequestKey,
        SemanticRuntimeHost, SemanticRuntimeSupervisor, StartFailure,
    },
    scan::BaselineScan,
    semantic::{
        AnalysisContext, AnalysisContextBinding, CapabilityReport, ProjectRootIdentity,
        SemanticBackendKind, SemanticCapability, ToolchainIdentity,
    },
    semantic_index::{
        ConfigBasis, CurrentInputs, ObsoleteReason, SemanticBasis, SemanticIndex,
        SemanticIndexError, SemanticOwner, SemanticState,
    },
    symbol::{AnalysisProfile, SymbolKind},
    trust::ProjectExecutionTrust,
};

/// A generous safety net for a wedged fake, never the synchronization
/// itself.
const PATIENCE: Duration = Duration::from_secs(30);

/// How many independent clients the task asks about.
const CLIENTS: usize = 5;

// ---------------------------------------------------------------------
// A deterministic fake backend
// ---------------------------------------------------------------------

/// Answers immediately, counts everything, and can be told to die.
struct FakeHost {
    healthy: AtomicBool,
    shutdowns: Arc<AtomicUsize>,
    own_shutdowns: AtomicUsize,
    executions: Arc<AtomicUsize>,
    crash_on_request: AtomicBool,
}

impl SemanticRuntimeHost for FakeHost {
    fn execute(
        &self,
        request: &RuntimeRequest,
        _cancel: &CancelToken,
    ) -> Result<RuntimeResponse, HostError> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        if self.crash_on_request.load(Ordering::SeqCst) {
            self.healthy.store(false, Ordering::SeqCst);
            return Err(HostError::new("backend exited"));
        }
        Ok(RuntimeResponse {
            payload: request.payload.clone(),
        })
    }

    fn health(&self) -> HostHealth {
        if self.healthy.load(Ordering::SeqCst) {
            HostHealth::Healthy
        } else {
            HostHealth::Crashed
        }
    }

    fn shutdown(&self) {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        self.own_shutdowns.fetch_add(1, Ordering::SeqCst);
    }

    fn concurrency(&self) -> HostConcurrency {
        HostConcurrency::Serial
    }
}

struct FakeLauncher {
    kind: SemanticBackendKind,
    launches: Arc<AtomicUsize>,
    shutdowns: Arc<AtomicUsize>,
    executions: Arc<AtomicUsize>,
    crash_on_request: Arc<AtomicBool>,
    hosts: Mutex<Vec<Arc<FakeHost>>>,
}

impl FakeLauncher {
    fn new(kind: SemanticBackendKind) -> Arc<Self> {
        Arc::new(Self {
            kind,
            launches: Arc::new(AtomicUsize::new(0)),
            shutdowns: Arc::new(AtomicUsize::new(0)),
            executions: Arc::new(AtomicUsize::new(0)),
            crash_on_request: Arc::new(AtomicBool::new(false)),
            hosts: Mutex::new(Vec::new()),
        })
    }

    fn launch_count(&self) -> usize {
        self.launches.load(Ordering::SeqCst)
    }

    fn shutdown_count(&self) -> usize {
        self.shutdowns.load(Ordering::SeqCst)
    }

    fn crash_next_request(&self) {
        self.crash_on_request.store(true, Ordering::SeqCst);
        for host in self
            .hosts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            host.crash_on_request.store(true, Ordering::SeqCst);
        }
    }
}

impl SemanticBackendLauncher for FakeLauncher {
    fn kind(&self) -> SemanticBackendKind {
        self.kind
    }

    fn launch(
        &self,
        _binding: &AnalysisContextBinding,
    ) -> Result<Arc<dyn SemanticRuntimeHost>, HostError> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        let host = Arc::new(FakeHost {
            healthy: AtomicBool::new(true),
            shutdowns: Arc::clone(&self.shutdowns),
            own_shutdowns: AtomicUsize::new(0),
            executions: Arc::clone(&self.executions),
            crash_on_request: AtomicBool::new(self.crash_on_request.load(Ordering::SeqCst)),
        });
        self.hosts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Arc::clone(&host));
        Ok(host)
    }
}

// ---------------------------------------------------------------------
// One indexed Workspace, with no backend anywhere near it
// ---------------------------------------------------------------------

struct Fixture {
    base: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn create(label: &str, uid: u8) -> Self {
        let base = env::temp_dir().join(format!(
            "brainprint-i4fleet-{label}-{}-{uid}",
            process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("workspace");
        fs::create_dir_all(root.join("src")).expect("workspace root");
        let fixture = Self { base, root };
        fixture.write(
            "src/app.py",
            "from src.other import stay\n\n\ndef go():\n    return stay()\n",
        );
        fixture.write("src/other.py", "def stay():\n    return 2\n");
        fixture.index("workspace-rev-1");
        fixture
    }

    fn db_path(&self) -> PathBuf {
        self.base.join("data").join("index.db")
    }

    fn write(&self, rel: &str, contents: &str) {
        fs::write(self.root.join(rel), contents).expect("fixture file");
    }

    fn index(&self, revision: &str) {
        BaselineScan::open(&self.db_path())
            .expect("index.db")
            .run_initial_scan(&self.root, &WorkspaceConfig::default(), revision)
            .expect("baseline scan");
    }

    fn resource(&self, rel: &str) -> brainprint_engine::resource::Resource {
        ResourceStore::open(&self.db_path())
            .expect("index.db")
            .list_active()
            .expect("resources")
            .into_iter()
            .find(|resource| resource.path_key == rel)
            .unwrap_or_else(|| panic!("{rel} is indexed"))
    }

    fn semantic(&self) -> SemanticIndex {
        SemanticIndex::open(&self.db_path()).expect("index.db")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

// ---------------------------------------------------------------------
// Contexts, without a client's name anywhere in one
// ---------------------------------------------------------------------

fn toolchain(environment: &str) -> ToolchainIdentity {
    ToolchainIdentity {
        backend_version: "1.0.0".to_owned(),
        backend_compatibility_class: "fake:1".to_owned(),
        environment_fingerprint: environment.to_owned(),
    }
}

fn context_in(workspace: u8, environment: &str) -> AnalysisContext {
    AnalysisContext {
        workspace: WorkspaceId::from_bytes([workspace; 16]),
        backend: SemanticBackendKind::Python,
        language: ResourceLanguage::Python,
        project_root: ProjectRootIdentity::Config(ResourceId::from_bytes([2; 16])),
        toolchain: toolchain(environment),
    }
}

fn binding_of(context: &AnalysisContext) -> AnalysisContextBinding {
    AnalysisContextBinding {
        context: context.clone(),
        project_root_rel: String::new(),
        config_file_rel: None,
    }
}

fn capabilities(context: &AnalysisContext) -> CapabilityReport {
    let mut report = CapabilityReport::new(context);
    report.declare(SemanticCapability::ImportBinding, Support::Supported);
    report
}

fn config() -> ConfigBasis {
    ConfigBasis::new().with("pyproject.toml", "sha256:config-a")
}

fn inputs(context: &AnalysisContext) -> CurrentInputs {
    CurrentInputs::new(context, &config(), &capabilities(context))
}

/// Publish one owner's semantic generation over `sources`.
fn publish(
    fixture: &Fixture,
    semantic: &SemanticIndex,
    context: &AnalysisContext,
    owner_rel: &str,
    sources: &[&str],
) -> Result<(), SemanticIndexError> {
    let owner = fixture.resource(owner_rel);
    let mut basis = SemanticBasis::new(context, &config(), owner.id);
    for rel in sources {
        let resource = fixture.resource(rel);
        basis = basis.with_source(resource.id, resource.resource_revision.clone());
    }
    let profile = AnalysisProfile::semantic(context, &capabilities(context));
    let candidate = semantic
        .begin_candidate(basis, profile, Support::Supported)
        .expect("candidate");
    semantic.publish(candidate, &inputs(context)).map(|_| ())
}

fn owner_of(fixture: &Fixture, context: &AnalysisContext, rel: &str) -> SemanticOwner {
    SemanticOwner::new(context.context_key(), fixture.resource(rel).id)
}

fn state_of(fixture: &Fixture, context: &AnalysisContext, rel: &str) -> SemanticState {
    fixture
        .semantic()
        .status(&owner_of(fixture, context, rel))
        .expect("status")
        .state
}

// ---------------------------------------------------------------------
// Concurrent publication stays owner-scoped
// ---------------------------------------------------------------------

/// Two owners refreshing at once are two answers, not one.
///
/// The correction task 8 made was to scope publication to
/// `(context_key, owner_resource)` rather than to the context. Under
/// concurrency that stops being a naming detail: if publishing owner A
/// marked the *context* current, then B finishing a moment later would
/// silently claim A's work too, and a failure in A would silently
/// withdraw B's.
#[test]
fn two_owners_publishing_at_once_never_answer_for_each_other() {
    for round in 0..8 {
        let fixture = Fixture::create("owners", 1);
        let context = context_in(1, "sha256:venv");

        // Two threads, two connections, one context, released together.
        let start = Arc::new(Barrier::new(2));
        thread::scope(|scope| {
            for rel in ["src/app.py", "src/other.py"] {
                let start = Arc::clone(&start);
                let fixture = &fixture;
                let context = &context;
                scope.spawn(move || {
                    let semantic = fixture.semantic();
                    start.wait();
                    publish(fixture, &semantic, context, rel, &[rel]).expect("publish");
                });
            }
        });

        assert_eq!(
            state_of(&fixture, &context, "src/app.py"),
            SemanticState::Current,
            "round {round}"
        );
        assert_eq!(
            state_of(&fixture, &context, "src/other.py"),
            SemanticState::Current,
            "round {round}"
        );
        assert_ne!(
            owner_of(&fixture, &context, "src/app.py").scope_key(),
            owner_of(&fixture, &context, "src/other.py").scope_key(),
            "round {round}: two owners, two scopes"
        );
    }
}

/// One owner failing is one owner failing.
#[test]
fn a_withdrawn_owner_leaves_the_other_exactly_as_it_was() {
    let fixture = Fixture::create("withdraw", 2);
    let context = context_in(1, "sha256:venv");
    let semantic = fixture.semantic();

    publish(&fixture, &semantic, &context, "src/app.py", &["src/app.py"]).expect("publish");
    publish(
        &fixture,
        &semantic,
        &context,
        "src/other.py",
        &["src/other.py"],
    )
    .expect("publish");

    semantic
        .mark_unavailable(
            &owner_of(&fixture, &context, "src/app.py"),
            "backend unavailable",
        )
        .expect("mark");

    assert_ne!(
        state_of(&fixture, &context, "src/app.py"),
        SemanticState::Current,
        "the owner that lost its backend says so"
    );
    assert_eq!(
        state_of(&fixture, &context, "src/other.py"),
        SemanticState::Current,
        "and the one that did not is untouched"
    );
}

/// A refresh in flight does not freeze the graph.
#[test]
fn an_unrelated_owner_stays_readable_while_another_refreshes() {
    let fixture = Fixture::create("read-during", 3);
    let context = context_in(1, "sha256:venv");
    let semantic = fixture.semantic();
    publish(
        &fixture,
        &semantic,
        &context,
        "src/other.py",
        &["src/other.py"],
    )
    .expect("publish");

    // A candidate is open: owner A is mid-refresh and has published
    // nothing. Holding one must change nothing a reader sees.
    let owner = fixture.resource("src/app.py");
    let mut basis = SemanticBasis::new(&context, &config(), owner.id);
    basis = basis.with_source(owner.id, owner.resource_revision.clone());
    let candidate = semantic
        .begin_candidate(
            basis,
            AnalysisProfile::semantic(&context, &capabilities(&context)),
            Support::Supported,
        )
        .expect("candidate");

    assert_eq!(
        state_of(&fixture, &context, "src/other.py"),
        SemanticState::Current,
        "an unrelated owner is readable while its neighbour refreshes"
    );
    assert_ne!(
        state_of(&fixture, &context, "src/app.py"),
        SemanticState::Current,
        "and the refreshing owner does not claim to be current yet"
    );

    // Structural truth is readable throughout: the relation index does
    // not wait on anyone's semantic candidate.
    let relations = RelationIndex::open(&fixture.db_path()).expect("index.db");
    assert!(
        relations
            .imports(owner.id)
            .expect("relations")
            .confirmed_count()
            > 0,
        "the structural import survives regardless of any semantic state"
    );

    semantic.discard(candidate, "test").expect("discard");
}

/// A late answer about an older Workspace cannot become current truth.
#[test]
fn an_answer_about_an_older_revision_cannot_publish_over_a_newer_one() {
    let fixture = Fixture::create("obsolete", 4);
    let context = context_in(1, "sha256:venv");
    let semantic = fixture.semantic();

    // Revision N: the analysis starts.
    let owner = fixture.resource("src/app.py");
    let basis = SemanticBasis::new(&context, &config(), owner.id)
        .with_source(owner.id, owner.resource_revision.clone());
    let late = semantic
        .begin_candidate(
            basis,
            AnalysisProfile::semantic(&context, &capabilities(&context)),
            Support::Supported,
        )
        .expect("candidate");

    // Revision N+1 happens while it is still running.
    fixture.write("src/app.py", "def go():\n    return 3\n");
    fixture.index("workspace-rev-2");

    // The N answer arrives last, and is refused.
    match semantic.publish(late, &inputs(&context)) {
        Err(SemanticIndexError::Obsolete { reason, .. }) => assert!(
            matches!(
                reason,
                ObsoleteReason::SourceRevisionMoved { .. }
                    | ObsoleteReason::WorkspaceRevisionMoved { .. }
            ),
            "refused for the reason it was actually obsolete: {reason:?}"
        ),
        other => panic!("a superseded answer must not publish: {other:?}"),
    }
    assert_ne!(
        state_of(&fixture, &context, "src/app.py"),
        SemanticState::Current,
        "and nothing was left claiming to be current"
    );

    // N+1 publishes normally. The backend was never the problem.
    publish(&fixture, &semantic, &context, "src/app.py", &["src/app.py"]).expect("publish");
    assert_eq!(
        state_of(&fixture, &context, "src/app.py"),
        SemanticState::Current
    );
}

// ---------------------------------------------------------------------
// Runtime state is not publication freshness
// ---------------------------------------------------------------------

/// Retiring a runtime does not touch what it published.
///
/// The two state machines are deliberately separate, and capacity is
/// the case where that separation earns its keep: a fleet under
/// pressure retires backends, and a retired backend must not cost the
/// Workspace the truth it already proved.
#[test]
fn a_retired_runtime_leaves_everything_it_published_exactly_where_it_was() {
    let fixture = Fixture::create("evict", 5);
    let context = context_in(1, "sha256:venv");
    let other = context_in(1, "sha256:other-venv");
    publish(
        &fixture,
        &fixture.semantic(),
        &context,
        "src/app.py",
        &["src/app.py"],
    )
    .expect("publish");

    let launcher = FakeLauncher::new(SemanticBackendKind::Python);
    let supervisor = SemanticRuntimeSupervisor::new(RuntimePolicy {
        max_live_runtimes: Some(1),
        ..RuntimePolicy::default()
    })
    .with_backend(Arc::clone(&launcher) as Arc<dyn SemanticBackendLauncher>);

    drop(supervisor.acquire(&binding_of(&context)).expect("start"));
    // A second context needs the only slot, so the first is retired.
    let _pressure = supervisor.acquire(&binding_of(&other)).expect("start");
    assert_eq!(
        supervisor.state(&context.context_key()),
        RuntimeState::Stopped,
        "the runtime is gone"
    );
    assert_eq!(launcher.shutdown_count(), 1);

    assert_eq!(
        state_of(&fixture, &context, "src/app.py"),
        SemanticState::Current,
        "and what it published is still current, because nothing it was proved against moved"
    );

    // Reading that truth does not wake anything up.
    let before = launcher.launch_count();
    for _ in 0..3 {
        assert_eq!(
            state_of(&fixture, &context, "src/app.py"),
            SemanticState::Current
        );
    }
    assert_eq!(
        launcher.launch_count(),
        before,
        "a persisted answer is an answer; it needs no backend to repeat it"
    );
    assert_eq!(
        supervisor.state(&context.context_key()),
        RuntimeState::Stopped,
        "and the backend stayed cold"
    );
}

/// A crash does not restore what was already invalidated.
#[test]
fn a_crash_cannot_make_invalidated_truth_current_again() {
    let fixture = Fixture::create("crash-truth", 6);
    let context = context_in(1, "sha256:venv");
    let semantic = fixture.semantic();
    publish(&fixture, &semantic, &context, "src/app.py", &["src/app.py"]).expect("publish");

    // The Workspace moves on while the backend is up: the clock is
    // ahead of what the publication was proved against, which is the
    // one thing that makes a persisted semantic answer stop being
    // current without anyone marking it.
    GenerationStore::open(&fixture.db_path())
        .expect("index.db")
        .set_current_workspace_revision("workspace-rev-2")
        .expect("clock");
    assert_ne!(
        state_of(&fixture, &context, "src/app.py"),
        SemanticState::Current,
        "a moved Workspace is not current, whatever the runtime is doing"
    );

    let launcher = FakeLauncher::new(SemanticBackendKind::Python);
    let supervisor = SemanticRuntimeSupervisor::new(RuntimePolicy::default())
        .with_backend(Arc::clone(&launcher) as Arc<dyn SemanticBackendLauncher>);
    let lease = supervisor.acquire(&binding_of(&context)).expect("start");
    launcher.crash_next_request();
    assert!(
        lease
            .execute(
                RuntimeRequest {
                    key: brainprint_engine::runtime::SemanticRequestKey {
                        context_key: context.context_key(),
                        capability: SemanticCapability::ImportBinding,
                        target: "boom".to_owned(),
                        basis_token: None,
                    },
                    priority: brainprint_engine::runtime::RequestPriority::Interactive,
                    payload: b"boom".to_vec(),
                },
                RequestOptions::default(),
            )
            .is_err()
    );
    assert!(supervisor.await_quiescent(&context.context_key(), PATIENCE));
    assert_eq!(
        supervisor.state(&context.context_key()),
        RuntimeState::Backoff
    );

    assert_ne!(
        state_of(&fixture, &context, "src/app.py"),
        SemanticState::Current,
        "a backend dying is not a reason to believe stale semantics again"
    );
}

/// Stopping the fleet does not touch the index it published into.
#[test]
fn shutting_the_fleet_down_leaves_the_semantic_index_where_it_was() {
    let fixture = Fixture::create("shutdown-truth", 7);
    let context = context_in(1, "sha256:venv");
    publish(
        &fixture,
        &fixture.semantic(),
        &context,
        "src/app.py",
        &["src/app.py"],
    )
    .expect("publish");
    let before = fs::metadata(fixture.db_path()).expect("index.db").len();

    let launcher = FakeLauncher::new(SemanticBackendKind::Python);
    let supervisor = SemanticRuntimeSupervisor::new(RuntimePolicy::default())
        .with_backend(Arc::clone(&launcher) as Arc<dyn SemanticBackendLauncher>);
    let _lease = supervisor.acquire(&binding_of(&context)).expect("start");
    supervisor.shutdown();

    assert!(matches!(
        supervisor.acquire(&binding_of(&context)),
        Err(StartFailure::ShuttingDown)
    ));
    assert_eq!(
        state_of(&fixture, &context, "src/app.py"),
        SemanticState::Current,
        "the daemon stopping is not the Workspace forgetting"
    );
    assert_eq!(
        fs::metadata(fixture.db_path()).expect("index.db").len(),
        before,
        "and nothing was written on the way out"
    );
}

// ---------------------------------------------------------------------
// Worktree isolation
// ---------------------------------------------------------------------

/// Two worktrees of one repository share nothing current.
///
/// Everything a cache would key on is identical here -- same relative
/// paths, same project root identity, same toolchain, same bytes -- and
/// the only difference is the `WorkspaceId`. That is the whole point:
/// saving one backend process by merging them would be answering one
/// checkout's question with another checkout's code.
#[test]
fn two_worktrees_never_answer_for_each_other() {
    let left = Fixture::create("worktree-left", 8);
    let right = Fixture::create("worktree-right", 9);
    let here = context_in(8, "sha256:venv");
    let there = context_in(9, "sha256:venv");

    assert_ne!(
        here.context_key(),
        there.context_key(),
        "identical everything but the worktree is still two contexts"
    );

    publish(
        &left,
        &left.semantic(),
        &here,
        "src/app.py",
        &["src/app.py"],
    )
    .expect("publish");
    assert_eq!(state_of(&left, &here, "src/app.py"), SemanticState::Current);
    assert_ne!(
        state_of(&right, &there, "src/app.py"),
        SemanticState::Current,
        "publishing in one worktree publishes nothing in the other"
    );

    // The sources then diverge, and each side answers for itself.
    right.write("src/other.py", "def stay():\n    return 99\n");
    right.index("workspace-rev-2");
    publish(
        &right,
        &right.semantic(),
        &there,
        "src/other.py",
        &["src/other.py"],
    )
    .expect("publish");
    assert_ne!(
        left.resource("src/other.py").content_hash,
        right.resource("src/other.py").content_hash,
        "the same path is a different file in each worktree"
    );

    // And a basis proved in one is refused by the other outright.
    let owner = right.resource("src/app.py");
    let foreign = SemanticBasis::new(&here, &config(), owner.id)
        .with_source(owner.id, owner.resource_revision.clone());
    let candidate = right
        .semantic()
        .begin_candidate(
            foreign,
            AnalysisProfile::semantic(&here, &capabilities(&here)),
            Support::Supported,
        )
        .expect("candidate");
    match right.semantic().publish(candidate, &inputs(&there)) {
        Err(SemanticIndexError::Obsolete {
            reason: ObsoleteReason::WorkspaceMismatch { .. },
            ..
        }) => {}
        other => panic!("one worktree's proof may not publish into another: {other:?}"),
    }
}

/// Neither do their runtimes, their dedupe, or their crashes.
#[test]
fn a_crash_in_one_worktree_is_invisible_in_the_other() {
    let launcher = FakeLauncher::new(SemanticBackendKind::Python);
    let supervisor = SemanticRuntimeSupervisor::new(RuntimePolicy::default())
        .with_backend(Arc::clone(&launcher) as Arc<dyn SemanticBackendLauncher>);
    let here = context_in(8, "sha256:venv");
    let there = context_in(9, "sha256:venv");

    let mine = supervisor.acquire(&binding_of(&here)).expect("start");
    let yours = supervisor.acquire(&binding_of(&there)).expect("start");
    assert_eq!(
        supervisor.live_runtime_count(),
        2,
        "two worktrees, two runtimes -- never shared to save memory"
    );

    launcher.crash_next_request();
    assert!(
        mine.execute(
            RuntimeRequest {
                key: brainprint_engine::runtime::SemanticRequestKey {
                    context_key: here.context_key(),
                    capability: SemanticCapability::ImportBinding,
                    target: "boom".to_owned(),
                    basis_token: None,
                },
                priority: brainprint_engine::runtime::RequestPriority::Interactive,
                payload: b"boom".to_vec(),
            },
            RequestOptions::default(),
        )
        .is_err()
    );
    assert!(supervisor.await_quiescent(&here.context_key(), PATIENCE));

    assert_eq!(supervisor.state(&here.context_key()), RuntimeState::Backoff);
    assert_eq!(
        yours.state(),
        RuntimeState::Ready,
        "the other worktree noticed nothing"
    );
    assert_eq!(
        supervisor
            .telemetry(&there.context_key())
            .expect("telemetry")
            .crashes,
        0
    );
    let fleet = supervisor.fleet_telemetry();
    assert_eq!(fleet.crashes, 1, "one crash in the fleet, not two");
    assert_eq!(fleet.backoff, 1);
    assert_eq!(fleet.ready, 1);
}

/// An identical question in two worktrees is two questions.
///
/// The dedupe key is built from the context key, and the context key
/// carries the `WorkspaceId`, so this is a property rather than a
/// policy -- but it is the property that would silently answer one
/// checkout out of another's backend if it ever stopped holding.
#[test]
fn an_identical_question_in_two_worktrees_is_never_deduplicated() {
    let here = context_in(8, "sha256:venv");
    let there = context_in(9, "sha256:venv");

    let ask = |context: &AnalysisContext| SemanticRequestKey {
        context_key: context.context_key(),
        capability: SemanticCapability::ImportBinding,
        target: "src/app.py::go".to_owned(),
        basis_token: Some("rev-1".to_owned()),
    };
    assert_ne!(
        ask(&here).dedupe_key(),
        ask(&there).dedupe_key(),
        "same capability, same target, same basis -- different checkout"
    );

    // And the runtimes they would run on are different entries, so
    // there is no shared in-flight map for them to meet in.
    let launcher = FakeLauncher::new(SemanticBackendKind::Python);
    let supervisor = SemanticRuntimeSupervisor::new(RuntimePolicy::default())
        .with_backend(Arc::clone(&launcher) as Arc<dyn SemanticBackendLauncher>);
    let mine = supervisor.acquire(&binding_of(&here)).expect("start");
    let yours = supervisor.acquire(&binding_of(&there)).expect("start");
    assert_ne!(mine.context_key(), yours.context_key());
    assert_eq!(launcher.launch_count(), 2);
}

/// A logical symbol cannot be shared between worktrees either.
///
/// The grouping that lets one type keep several declarations is keyed
/// by the context, and the context carries the Workspace. Two checkouts
/// of one repository declaring the same fully qualified name in the
/// same project file are still two types.
#[test]
fn a_logical_symbol_belongs_to_one_workspace() {
    let here = context_in(8, "sha256:venv");
    let there = context_in(9, "sha256:venv");
    let identity = |context: &AnalysisContext| LogicalIdentity {
        context_key: context.context_key(),
        project_key: "src/Core/Core.csproj".to_owned(),
        qualified_name: "Core.Runner".to_owned(),
        kind: SymbolKind::Class,
        arity: 0,
        discriminator: String::new(),
    };
    assert_ne!(
        identity(&here).fingerprint(),
        identity(&there).fingerprint(),
        "everything but the worktree is identical, and that is enough"
    );
}

/// React is not a language and has no backend of its own.
#[test]
fn react_registers_no_backend() {
    // The closed vocabulary is the proof: there is no React kind to
    // register a launcher for, and JSX/TSX are served by the TS/JS
    // backend as ordinary TypeScript.
    let families = [
        SemanticBackendKind::Python,
        SemanticBackendKind::TypeScriptJavaScript,
        SemanticBackendKind::Svelte,
        SemanticBackendKind::CSharp,
        SemanticBackendKind::Rust,
    ];
    for family in families {
        assert!(
            !format!("{family}").to_ascii_lowercase().contains("react"),
            "{family} is a backend family; React is not"
        );
    }
    assert!(
        SemanticBackendKind::TypeScriptJavaScript
            .languages()
            .contains(&ResourceLanguage::TypeScript),
        "a .tsx file is TypeScript, and that is the whole of React's backend story"
    );
}

// ---------------------------------------------------------------------
// Trust isolation
// ---------------------------------------------------------------------

/// Trust is an input to a semantic world, not a switch on the fleet.
///
/// C# and Rust share [`ProjectExecutionTrust`], and what it changes is
/// what the backend is allowed to read -- which changes the environment
/// the analysis was produced under, which is part of the context. Two
/// trust settings are therefore two contexts, and the supervisor has no
/// opinion about either.
#[test]
fn trusted_and_untrusted_worlds_are_different_contexts() {
    // The environment fingerprint is what trust moves: an untrusted
    // context has not read the projects a trusted one has.
    let trusted = context_in(1, "sha256:projects-loaded");
    let untrusted = context_in(1, "sha256:no-projects");
    assert_ne!(trusted.context_key(), untrusted.context_key());

    let launcher = FakeLauncher::new(SemanticBackendKind::Python);
    let supervisor = SemanticRuntimeSupervisor::new(RuntimePolicy::default())
        .with_backend(Arc::clone(&launcher) as Arc<dyn SemanticBackendLauncher>);

    let careful = supervisor.acquire(&binding_of(&untrusted)).expect("start");
    let permitted = supervisor.acquire(&binding_of(&trusted)).expect("start");
    assert_ne!(careful.context_key(), permitted.context_key());
    assert_eq!(
        launcher.launch_count(),
        2,
        "one client asking for trusted semantics does not promote another's world"
    );

    // And a result proved in one may not be published into the other.
    let fixture = Fixture::create("trust", 10);
    publish(
        &fixture,
        &fixture.semantic(),
        &trusted,
        "src/app.py",
        &["src/app.py"],
    )
    .expect("publish");
    assert_eq!(
        state_of(&fixture, &trusted, "src/app.py"),
        SemanticState::Current
    );
    assert_ne!(
        state_of(&fixture, &untrusted, "src/app.py"),
        SemanticState::Current,
        "the untrusted world was told nothing by the trusted one"
    );
}

/// There is nowhere to put a fleet-wide trust flag, and that is checked.
#[test]
fn trust_belongs_to_a_context_and_the_supervisor_has_no_opinion() {
    // `RuntimePolicy` is the whole of what a supervisor is configured
    // with. If trust ever became fleet-global it would have to live
    // here, and this comparison would stop compiling or stop holding.
    let policy = RuntimePolicy::default();
    assert_eq!(policy, RuntimePolicy { ..policy });
    assert_eq!(
        ProjectExecutionTrust::default(),
        ProjectExecutionTrust::Untrusted,
        "and the default answer is still no"
    );
    assert!(!ProjectExecutionTrust::Untrusted.may_load_projects());
    assert!(ProjectExecutionTrust::Trusted.may_load_projects());
}

// ---------------------------------------------------------------------
// Five anonymous clients over one shared fleet
// ---------------------------------------------------------------------

/// Five clients, five families, one supervisor, no deadlock.
///
/// Fakes rather than real servers, so this runs always: what is being
/// proved is the topology, and the topology does not know which
/// language is behind a launcher. The real installed fleet is the
/// `#[ignore]`d test below.
#[test]
fn five_clients_over_five_families_share_one_supervisor() {
    let families = [
        (SemanticBackendKind::Python, ResourceLanguage::Python),
        (
            SemanticBackendKind::TypeScriptJavaScript,
            ResourceLanguage::TypeScript,
        ),
        (SemanticBackendKind::Svelte, ResourceLanguage::Svelte),
        (SemanticBackendKind::CSharp, ResourceLanguage::CSharp),
        (SemanticBackendKind::Rust, ResourceLanguage::Rust),
    ];
    let launchers: Vec<Arc<FakeLauncher>> = families
        .iter()
        .map(|(kind, _)| FakeLauncher::new(*kind))
        .collect();
    let mut supervisor = SemanticRuntimeSupervisor::new(RuntimePolicy::default());
    for launcher in &launchers {
        supervisor =
            supervisor.with_backend(Arc::clone(launcher) as Arc<dyn SemanticBackendLauncher>);
    }
    let supervisor = Arc::new(supervisor);

    let contexts: Vec<AnalysisContext> = families
        .iter()
        .map(|(kind, language)| AnalysisContext {
            workspace: WorkspaceId::from_bytes([1; 16]),
            backend: *kind,
            language: *language,
            project_root: ProjectRootIdentity::Key(format!("{kind}")),
            toolchain: toolchain("sha256:mixed"),
        })
        .collect();

    // Each client takes one family, and one of them also takes a
    // second lease on a family another client already holds.
    let start = Arc::new(Barrier::new(CLIENTS));
    thread::scope(|scope| {
        for (index, context) in contexts.iter().enumerate() {
            let supervisor = Arc::clone(&supervisor);
            let start = Arc::clone(&start);
            let shared = contexts[0].clone();
            scope.spawn(move || {
                start.wait();
                let own = supervisor.acquire(&binding_of(context)).expect("start");
                let also = supervisor.acquire(&binding_of(&shared)).expect("share");
                assert!(
                    also.execute(
                        RuntimeRequest {
                            key: brainprint_engine::runtime::SemanticRequestKey {
                                context_key: shared.context_key(),
                                capability: SemanticCapability::ImportBinding,
                                target: format!("client-{index}"),
                                basis_token: None,
                            },
                            priority: brainprint_engine::runtime::RequestPriority::Interactive,
                            payload: b"ask".to_vec(),
                        },
                        RequestOptions::with_timeout(PATIENCE),
                    )
                    .is_ok()
                );
                drop((own, also));
            });
        }
    });

    assert_eq!(
        supervisor.live_runtime_count(),
        families.len(),
        "five families, five runtimes"
    );
    for launcher in &launchers {
        assert_eq!(
            launcher.launch_count(),
            1,
            "and no family was started twice, however many clients asked"
        );
    }
    let fleet = supervisor.fleet_telemetry();
    assert_eq!(fleet.known_contexts, families.len());
    assert_eq!(fleet.starts_attempted, families.len() as u64);
    assert_eq!(fleet.starts_succeeded, families.len() as u64);
    assert_eq!(fleet.crashes, 0);
    assert_eq!(fleet.active_leases, 0, "every client let go");
    assert_eq!(
        fleet.resource_usage_unknown,
        families.len(),
        "no fake measures memory, and none of them pretends to"
    );
    assert_eq!(fleet.known_rss_bytes, None);
}

// =====================================================================
// The real fleet
// =====================================================================

use brainprint_engine::{
    csharp_semantic::{CSharpInstall, CSharpLauncher},
    python_semantic::{PyrightInstall, PythonLauncher, PythonSettings},
    rust_semantic::{RustInstall, RustLauncher},
    svelte_semantic::{SvelteInstall, SvelteLauncher},
    typescript_semantic::{TypeScriptInstall, TypeScriptLauncher},
};

fn spike_root(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../scripts/{name}"))
}

fn fixture_root(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../fixtures/workspaces/{name}"))
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("destination");
    for entry in fs::read_dir(from).expect("read fixture") {
        let entry = entry.expect("entry");
        let name = entry.file_name();
        if name == "target" || name == "build-rs-ran.marker" || name == "node_modules" {
            continue;
        }
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy");
        }
    }
}

/// One real family: its launcher, its context, and the Workspace copy
/// it was measured over.
struct Family {
    label: &'static str,
    context: AnalysisContext,
    binding: AnalysisContextBinding,
    launcher: Arc<dyn SemanticBackendLauncher>,
}

/// Everything installed on this machine, or nothing.
///
/// A family whose toolchain is absent is left out rather than faked,
/// and the test says which ones it found. The base directory is
/// returned so the whole fleet's scratch space can be removed at once.
fn installed_families(base: &Path) -> Vec<Family> {
    let mut families = Vec::new();
    let mut uid = 60_u8;
    let mut prepare = |label: &'static str, fixture: &str| -> Option<(PathBuf, PathBuf, u8)> {
        let workspace = base.join(label).join("workspace");
        copy_tree(&fixture_root(fixture), &workspace);
        let db_path = base.join(label).join("data").join("index.db");
        BaselineScan::open(&db_path)
            .ok()?
            .run_initial_scan(&workspace, &WorkspaceConfig::default(), "workspace-rev-1")
            .ok()?;
        uid += 1;
        Some((workspace, db_path, uid))
    };

    if let Ok(install) = PyrightInstall::locate(&spike_root("python_semantic_spike"), "node")
        && let Some((workspace, _db, uid)) = prepare("python", "python-semantic-spike")
    {
        let settings = PythonSettings::default();
        let context = AnalysisContext {
            workspace: WorkspaceId::from_bytes([uid; 16]),
            backend: SemanticBackendKind::Python,
            language: ResourceLanguage::Python,
            project_root: ProjectRootIdentity::Key("fleet-python".to_owned()),
            toolchain: brainprint_engine::python_semantic::toolchain_identity(
                &install,
                &brainprint_engine::python_semantic::lifecycle::environment_identity(
                    &workspace, &settings,
                ),
            ),
        };
        families.push(Family {
            label: "python",
            binding: AnalysisContextBinding {
                context: context.clone(),
                project_root_rel: String::new(),
                config_file_rel: Some("pyrightconfig.json".to_owned()),
            },
            context,
            launcher: Arc::new(PythonLauncher::new(install, workspace, settings)),
        });
    }

    if let Ok(install) = TypeScriptInstall::locate(&spike_root("typescript_semantic_spike"))
        && let Some((workspace, db_path, uid)) = prepare("typescript", "typescript-semantic-spike")
    {
        let index = SemanticIndex::open(&db_path).expect("index.db");
        if let Ok(environment) =
            brainprint_engine::typescript_semantic::lifecycle::environment_identity(
                index.connection(),
                &workspace,
                "",
                &install.manifest_version,
            )
        {
            let context = AnalysisContext {
                workspace: WorkspaceId::from_bytes([uid; 16]),
                backend: SemanticBackendKind::TypeScriptJavaScript,
                language: ResourceLanguage::TypeScript,
                project_root: ProjectRootIdentity::Key("fleet-typescript".to_owned()),
                toolchain: brainprint_engine::typescript_semantic::toolchain_identity(
                    &install,
                    &environment,
                ),
            };
            families.push(Family {
                label: "typescript",
                binding: AnalysisContextBinding {
                    context: context.clone(),
                    project_root_rel: workspace.to_string_lossy().into_owned(),
                    config_file_rel: Some("tsconfig.json".to_owned()),
                },
                context,
                launcher: Arc::new(TypeScriptLauncher::new(install)),
            });
        }
    }

    if let Ok(install) = SvelteInstall::locate(&spike_root("svelte_semantic_spike"))
        && let Some((workspace, db_path, uid)) = prepare("svelte", "svelte-semantic-spike")
    {
        let index = SemanticIndex::open(&db_path).expect("index.db");
        if let Ok(environment) = brainprint_engine::svelte_semantic::lifecycle::environment_identity(
            index.connection(),
            &workspace,
            "",
            &install,
        ) {
            let context = AnalysisContext {
                workspace: WorkspaceId::from_bytes([uid; 16]),
                backend: SemanticBackendKind::Svelte,
                language: ResourceLanguage::Svelte,
                project_root: ProjectRootIdentity::Key("fleet-svelte".to_owned()),
                toolchain: brainprint_engine::svelte_semantic::toolchain_identity(
                    &install,
                    &environment,
                ),
            };
            families.push(Family {
                label: "svelte",
                binding: AnalysisContextBinding {
                    context: context.clone(),
                    project_root_rel: workspace.to_string_lossy().into_owned(),
                    config_file_rel: None,
                },
                context,
                launcher: Arc::new(SvelteLauncher::new(
                    install,
                    env::var("BRAINPRINT_NODE").unwrap_or_else(|_| "node".to_owned()),
                )),
            });
        }
    }

    if let Ok(install) = CSharpInstall::locate(&spike_root("csharp_semantic_spike"))
        && let Some((workspace, db_path, uid)) = prepare("csharp", "csharp-semantic-spike")
    {
        // Untrusted on purpose: this test is about topology, and
        // loading a project is the one thing trust gates.
        let trust = ProjectExecutionTrust::Untrusted;
        let index = SemanticIndex::open(&db_path).expect("index.db");
        let projects = brainprint_engine::csharp_semantic::lifecycle::discover_projects_under(
            index.connection(),
            trust,
            Some(&workspace),
        )
        .expect("projects");
        if let Ok(environment) =
            brainprint_engine::csharp_semantic::lifecycle::environment_identity(&install, &projects)
        {
            let context = AnalysisContext {
                workspace: WorkspaceId::from_bytes([uid; 16]),
                backend: SemanticBackendKind::CSharp,
                language: ResourceLanguage::CSharp,
                project_root: ProjectRootIdentity::Key("fleet-csharp".to_owned()),
                toolchain: brainprint_engine::csharp_semantic::toolchain_identity(
                    &install,
                    &environment,
                ),
            };
            families.push(Family {
                label: "csharp",
                binding: AnalysisContextBinding {
                    context: context.clone(),
                    project_root_rel: workspace.to_string_lossy().into_owned(),
                    config_file_rel: None,
                },
                context,
                launcher: Arc::new(CSharpLauncher::new(
                    install,
                    trust,
                    base.join("csharp").join("server-logs"),
                )),
            });
        }
    }

    if let Some(install) = rust_install()
        && let Some((workspace, db_path, uid)) = prepare("rust", "rust-semantic-spike")
    {
        let trust = ProjectExecutionTrust::Untrusted;
        let index = SemanticIndex::open(&db_path).expect("index.db");
        let packages = brainprint_engine::rust_semantic::lifecycle::discover_packages_under(
            index.connection(),
            trust,
            Some(&workspace),
        )
        .expect("packages");
        if let Ok(environment) =
            brainprint_engine::rust_semantic::lifecycle::environment_identity(&install, &packages)
        {
            let context = AnalysisContext {
                workspace: WorkspaceId::from_bytes([uid; 16]),
                backend: SemanticBackendKind::Rust,
                language: ResourceLanguage::Rust,
                project_root: ProjectRootIdentity::Key("fleet-rust".to_owned()),
                toolchain: brainprint_engine::rust_semantic::toolchain_identity(
                    &install,
                    &environment,
                ),
            };
            families.push(Family {
                label: "rust",
                binding: AnalysisContextBinding {
                    context: context.clone(),
                    project_root_rel: workspace.to_string_lossy().into_owned(),
                    config_file_rel: None,
                },
                context,
                launcher: Arc::new(RustLauncher::new(install, trust)),
            });
        }
    }

    families
}

/// The rust-analyzer this machine has, found the one way a *test* may
/// ask. Production Brainprint is handed the path.
fn rust_install() -> Option<RustInstall> {
    let which = process::Command::new("rustup")
        .args(["which", "rust-analyzer"])
        .output()
        .ok()?;
    if !which.status.success() {
        return None;
    }
    RustInstall::at(String::from_utf8_lossy(&which.stdout).trim()).ok()
}

/// One supervisor, every installed family, five concurrent clients.
///
/// The claim: different semantic worlds coexist without interfering,
/// and the same world is never started twice however many clients ask
/// for it -- against real processes rather than fakes.
#[test]
#[ignore = "needs the installed Python, TypeScript, Svelte, C# and Rust backends"]
fn one_supervisor_serves_every_installed_backend_family() {
    let base = env::temp_dir().join(format!("brainprint-i4fleet-real-{}", process::id()));
    let _ = fs::remove_dir_all(&base);
    let families = installed_families(&base);
    if families.is_empty() {
        println!("skipped: no semantic backend is installed");
        let _ = fs::remove_dir_all(&base);
        return;
    }
    println!(
        "fleet: {}",
        families
            .iter()
            .map(|family| family.label)
            .collect::<Vec<_>>()
            .join(", ")
    );

    let mut supervisor = SemanticRuntimeSupervisor::new(RuntimePolicy::default());
    for family in &families {
        supervisor = supervisor.with_backend(Arc::clone(&family.launcher));
    }
    let supervisor = Arc::new(supervisor);

    // Every context must be distinct, including Svelte's own TypeScript
    // and the standalone TS/JS backend: one is a container's toolchain
    // and the other is not, and merging them would answer a `.svelte`
    // question out of the wrong project.
    let mut keys: Vec<String> = families
        .iter()
        .map(|family| family.context.context_key())
        .collect();
    keys.sort();
    keys.dedup();
    assert_eq!(
        keys.len(),
        families.len(),
        "every installed family is its own AnalysisContext"
    );

    // Five clients, released together, each taking its own family and
    // then a second lease on the first one.
    let clients = CLIENTS.max(families.len());
    let start = Arc::new(Barrier::new(clients));
    thread::scope(|scope| {
        for index in 0..clients {
            let supervisor = Arc::clone(&supervisor);
            let start = Arc::clone(&start);
            let mine = families[index % families.len()].binding.clone();
            let shared = families[0].binding.clone();
            scope.spawn(move || {
                start.wait();
                let own = supervisor.acquire(&mine).expect("a real backend starts");
                let also = supervisor.acquire(&shared).expect("a shared context");
                assert_eq!(own.state(), RuntimeState::Ready);
                assert_eq!(also.state(), RuntimeState::Ready);
                drop((own, also));
            });
        }
    });

    assert_eq!(
        supervisor.live_runtime_count(),
        families.len(),
        "one process per semantic world, whatever the client count"
    );
    let fleet = supervisor.fleet_telemetry();
    assert_eq!(fleet.known_contexts, families.len());
    assert_eq!(
        fleet.starts_attempted, fleet.starts_succeeded,
        "nothing failed to start"
    );
    assert_eq!(
        fleet.starts_succeeded,
        families.len() as u64,
        "and nothing started twice: {clients} clients, {} runtimes",
        families.len()
    );
    assert_eq!(fleet.crashes, 0);
    assert_eq!(fleet.restart_attempts, 0);
    assert_eq!(fleet.active_leases, 0);
    println!(
        "live={} known_rss={:?} unmeasured={}",
        fleet.live_runtimes, fleet.known_rss_bytes, fleet.resource_usage_unknown
    );

    supervisor.shutdown();
    assert_eq!(supervisor.live_runtime_count(), 0);
    for family in &families {
        assert_eq!(
            supervisor.state(&family.context.context_key()),
            RuntimeState::Stopped,
            "{}: no runtime stays READY behind a stopped supervisor",
            family.label
        );
    }
    drop(families);
    let _ = fs::remove_dir_all(&base);
}

/// Trusted and untrusted C# and Rust are different semantic worlds.
///
/// Trust decides whether the backend may read the Workspace's own
/// project definitions, which decides what environment the analysis was
/// produced under, which is part of the context. So this is not a flag
/// the fleet carries: it is an input, and two answers to it are two
/// contexts that can never be handed each other's results.
#[test]
#[ignore = "needs the installed C# and Rust backends"]
fn trusted_and_untrusted_csharp_and_rust_never_reuse_each_other() {
    let base = env::temp_dir().join(format!("brainprint-i4fleettrust-{}", process::id()));
    let _ = fs::remove_dir_all(&base);
    let mut checked = 0;

    if let Ok(install) = CSharpInstall::locate(&spike_root("csharp_semantic_spike")) {
        let workspace = base.join("csharp").join("workspace");
        copy_tree(&fixture_root("csharp-semantic-spike"), &workspace);
        let db_path = base.join("csharp").join("data").join("index.db");
        BaselineScan::open(&db_path)
            .expect("index.db")
            .run_initial_scan(&workspace, &WorkspaceConfig::default(), "workspace-rev-1")
            .expect("baseline scan");
        let index = SemanticIndex::open(&db_path).expect("index.db");
        let mut keys = Vec::new();
        for trust in [
            ProjectExecutionTrust::Untrusted,
            ProjectExecutionTrust::Trusted,
        ] {
            let projects = brainprint_engine::csharp_semantic::lifecycle::discover_projects_under(
                index.connection(),
                trust,
                Some(&workspace),
            )
            .expect("projects");
            let environment = brainprint_engine::csharp_semantic::lifecycle::environment_identity(
                &install, &projects,
            )
            .expect("environment");
            keys.push(
                brainprint_engine::csharp_semantic::toolchain_identity(&install, &environment)
                    .fingerprint(),
            );
        }
        assert_ne!(
            keys[0], keys[1],
            "an untrusted C# world has not read the projects a trusted one has"
        );
        checked += 1;
    }

    if let Some(install) = rust_install() {
        let workspace = base.join("rust").join("workspace");
        copy_tree(&fixture_root("rust-semantic-spike"), &workspace);
        let db_path = base.join("rust").join("data").join("index.db");
        BaselineScan::open(&db_path)
            .expect("index.db")
            .run_initial_scan(&workspace, &WorkspaceConfig::default(), "workspace-rev-1")
            .expect("baseline scan");
        let index = SemanticIndex::open(&db_path).expect("index.db");
        let mut keys = Vec::new();
        for trust in [
            ProjectExecutionTrust::Untrusted,
            ProjectExecutionTrust::Trusted,
        ] {
            let packages = brainprint_engine::rust_semantic::lifecycle::discover_packages_under(
                index.connection(),
                trust,
                Some(&workspace),
            )
            .expect("packages");
            let environment = brainprint_engine::rust_semantic::lifecycle::environment_identity(
                &install, &packages,
            )
            .expect("environment");
            keys.push(
                brainprint_engine::rust_semantic::toolchain_identity(&install, &environment)
                    .fingerprint(),
            );
        }
        assert_ne!(
            keys[0], keys[1],
            "an untrusted Rust world has not read the manifests a trusted one has"
        );
        checked += 1;
    }

    if checked == 0 {
        println!("skipped: neither the C# nor the Rust backend is installed");
    }
    let _ = fs::remove_dir_all(&base);
}

/// Capacity applies to real backends the same way it applies to fakes.
#[test]
#[ignore = "needs at least two installed semantic backends"]
fn a_real_fleet_under_a_cap_retires_rather_than_refusing_everyone() {
    let base = env::temp_dir().join(format!("brainprint-i4fleetcap-{}", process::id()));
    let _ = fs::remove_dir_all(&base);
    let families = installed_families(&base);
    if families.len() < 2 {
        println!("skipped: fewer than two semantic backends are installed");
        let _ = fs::remove_dir_all(&base);
        return;
    }

    let mut supervisor = SemanticRuntimeSupervisor::new(RuntimePolicy {
        max_live_runtimes: Some(1),
        ..RuntimePolicy::default()
    });
    for family in &families {
        supervisor = supervisor.with_backend(Arc::clone(&family.launcher));
    }

    drop(supervisor.acquire(&families[0].binding).expect("start"));
    assert_eq!(supervisor.live_runtime_count(), 1);

    // The second family needs the only slot. The first is unleased, so
    // it makes room rather than refusing.
    let held = supervisor.acquire(&families[1].binding).expect("start");
    assert_eq!(supervisor.live_runtime_count(), 1, "the cap held");
    assert_eq!(
        supervisor.state(&families[0].context.context_key()),
        RuntimeState::Stopped
    );
    assert_eq!(supervisor.fleet_telemetry().capacity_evictions, 1);

    // Now the only slot is protected, and a third family is refused
    // rather than taking it.
    if families.len() > 2 {
        match supervisor.acquire(&families[2].binding) {
            Err(StartFailure::Capacity { limit, live }) => {
                assert_eq!(limit, 1);
                assert_eq!(live, 2);
            }
            other => panic!("expected a capacity refusal, got {other:?}"),
        }
    }
    drop(held);
    supervisor.shutdown();
    drop(families);
    let _ = fs::remove_dir_all(&base);
}
