//! Rust semantic acceptance, against the real rust-analyzer
//! (#19 task 13).
//!
//! What this proves that the scripted tests cannot: that the answers the
//! adapter is built around are the answers rust-analyzer actually
//! gives, over a real Cargo workspace, driven through Brainprint's own
//! launcher, host and protocol code.
//!
//! Every fact travels the whole tier and is read back through the
//! ordinary APIs:
//!
//! ```text
//! .rs → LSP → adapter → evidence → publication → merge → query API
//! ```
//!
//! ```sh
//! ./scripts/rust_semantic_spike/install.sh
//! cargo test -p brainprint-engine --test i4_rust_acceptance -- --ignored --nocapture
//! ```
//!
//! ## Why these tests load a Cargo project, and what that does not mean
//!
//! Reading a manifest is reading what the Workspace wrote, so it is
//! trust-gated. These tests pass [`ProjectExecutionTrust::Trusted`]
//! because the fixture is a committed, reviewed, three-package
//! workspace — an explicit decision about a known tree, which is the
//! only kind of trust decision this design admits.
//!
//! Trusted still does not mean *executing* it. The fixture carries a
//! `build.rs` that writes a marker file if it ever runs, and every test
//! here asserts the marker is absent afterwards.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process,
    sync::Arc,
    time::Duration,
};

use brainprint_core::WorkspaceId;
use brainprint_engine::{
    config::WorkspaceConfig,
    graph::{GraphEndpoint, RelationKind},
    impact::{Budget, ImpactIntent, ImpactTraversal},
    prepare::InspectPreparer,
    related_tests::RelatedTests,
    relations::{Direction, RelationIndex},
    resolution::Dispatch,
    resource::{Resource, ResourceLanguage, ResourceStore},
    runtime::{
        CancelToken, RequestFailure, RuntimePolicy, SemanticBackendLauncher, SemanticRuntimeHost,
        SemanticRuntimeSupervisor,
    },
    rust_semantic::{
        ProjectExecutionTrust, RefreshRequest, RustHost, RustInstall, RustLauncher, RustQueries,
        capability_report, lifecycle,
        protocol::{RustRequest, RustResponse, path_to_uri},
        refresh_resource, toolchain_identity,
    },
    scan::BaselineScan,
    semantic::{
        AnalysisContext, AnalysisContextBinding, ProjectRootIdentity, SemanticBackendKind,
        SemanticCapability,
    },
    semantic_index::SemanticIndex,
    symbol::{Symbol, SymbolStore},
};
use rusqlite::Connection;

/// The module whose trait implementations the fixture turns on.
const RUNNER: &str = "crates/core/src/runner.rs";
/// The crate that declares the traits.
const CONTRACTS: &str = "crates/contracts/src/lib.rs";
/// The consumer.
const MAIN: &str = "crates/app/src/main.rs";

/// How long the initial project load may take.
///
/// Generous on purpose: a cold `cargo metadata` on a loaded machine is
/// slow, and a flaky timeout would turn a real signal into noise. The
/// barrier is still a signal — it returns the moment the server
/// announces it has settled.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(240);

// ---------------------------------------------------------------------
// Fixture plumbing
// ---------------------------------------------------------------------

fn fixture_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/workspaces/rust-semantic-spike")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("destination");
    for entry in fs::read_dir(from).expect("read fixture") {
        let entry = entry.expect("entry");
        // Never carry build output, and never carry the marker a
        // developer's own `cargo test` in the fixture would leave: the
        // trust assertion must be about *this* run.
        let name = entry.file_name();
        if name == "target" || name == "build-rs-ran.marker" {
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

/// Asks the live host directly.
struct HostQueries<'a> {
    host: &'a RustHost,
    cancel: CancelToken,
}

impl RustQueries for HostQueries<'_> {
    fn call(&self, request: &RustRequest) -> Result<RustResponse, RequestFailure> {
        self.host
            .call(request, &self.cancel)
            .map_err(RequestFailure::Backend)
    }
}

impl lifecycle::DocumentVersions for HostQueries<'_> {
    fn next_document_version(&self) -> i64 {
        self.host.next_document_version()
    }

    fn exchange_text(&self, uri: &str, text: &str) -> Option<String> {
        self.host.exchange_text(uri, text)
    }
}

impl lifecycle::QuiescenceBarrier for HostQueries<'_> {
    fn settlings_seen(&self) -> u64 {
        self.host.load_completions()
    }

    fn wait_for_quiescent(&self, seen: u64) -> Result<usize, String> {
        if self.host.wait_for_quiescent(seen, SETTLE_TIMEOUT) {
            Ok(1)
        } else {
            Err(format!(
                "the server did not settle within {SETTLE_TIMEOUT:?}"
            ))
        }
    }
}

/// One indexed Cargo workspace with one rust-analyzer behind it.
struct Slice {
    workspace: PathBuf,
    db_path: PathBuf,
    base: PathBuf,
    /// Where the fixture's `build.rs` writes if it ever runs.
    marker: PathBuf,
    context: AnalysisContext,
    launcher: Arc<RustLauncher>,
    trust: ProjectExecutionTrust,
}

impl Slice {
    fn open(label: &str, uid: u8, install: &RustInstall, trust: ProjectExecutionTrust) -> Self {
        let base = env::temp_dir().join(format!("brainprint-i4rust-{label}-{}", process::id()));
        let _ = fs::remove_dir_all(&base);
        let workspace = base.join("workspace");
        copy_tree(&fixture_source(), &workspace);
        // The build script writes beside its own manifest, so the
        // check needs no environment variable and works wherever the
        // fixture was copied to.
        let marker = workspace.join("crates/core/build-rs-ran.marker");

        let db_path = base.join("data").join("index.db");
        BaselineScan::open(&db_path)
            .expect("index.db")
            .run_initial_scan(&workspace, &WorkspaceConfig::default(), "workspace-rev-1")
            .expect("baseline scan");

        let environment = {
            let index = SemanticIndex::open(&db_path).expect("index.db");
            let packages =
                lifecycle::discover_packages_under(index.connection(), trust, Some(&workspace))
                    .expect("packages");
            lifecycle::environment_identity(install, &packages).expect("environment")
        };
        let context = AnalysisContext {
            workspace: WorkspaceId::from_bytes([uid; 16]),
            backend: SemanticBackendKind::Rust,
            language: ResourceLanguage::Rust,
            project_root: ProjectRootIdentity::Key(format!("rust-spike-{label}")),
            toolchain: toolchain_identity(install, &environment),
        };
        Self {
            workspace,
            db_path,
            base,
            marker,
            context,
            launcher: Arc::new(RustLauncher::new(install.clone(), trust)),
            trust,
        }
    }

    fn binding(&self) -> AnalysisContextBinding {
        AnalysisContextBinding {
            context: self.context.clone(),
            project_root_rel: self.workspace.to_string_lossy().into_owned(),
            config_file_rel: None,
        }
    }

    fn packages(&self) -> lifecycle::RustProjectConfig {
        let index = SemanticIndex::open(&self.db_path).expect("index.db");
        lifecycle::discover_packages_under(index.connection(), self.trust, Some(&self.workspace))
            .expect("packages")
    }

    fn rescan(&self, revision: &str) {
        BaselineScan::open(&self.db_path)
            .expect("index.db")
            .run_initial_scan(&self.workspace, &WorkspaceConfig::default(), revision)
            .expect("baseline scan");
    }

    /// Withdraw every semantic contribution this context published.
    ///
    /// The documented order: withdraw, then replace structurally, then
    /// refresh. `semantic_evidence.relation_id` has no cascade, and
    /// this harness replays a whole baseline scan, so every owner is
    /// replaced at once.
    fn withdraw_all(&self) {
        let index = SemanticIndex::open(&self.db_path).expect("index.db");
        let owners: std::collections::BTreeSet<_> = index
            .owners_of_context(&self.context.context_key())
            .expect("owners")
            .into_iter()
            .collect();
        lifecycle::withdraw_affected(&index, &owners, "TEST_STRUCTURAL_REPLACEMENT")
            .expect("withdraw");
    }

    fn resources(&self) -> Vec<Resource> {
        ResourceStore::open(&self.db_path)
            .expect("index.db")
            .list_active()
            .expect("resources")
    }

    fn resource(&self, rel: &str) -> Resource {
        self.resources()
            .into_iter()
            .find(|resource| resource.path_key == rel)
            .unwrap_or_else(|| panic!("{rel} is indexed"))
    }

    fn sources(&self) -> Vec<String> {
        let mut found: Vec<String> = self
            .resources()
            .into_iter()
            .filter(|resource| resource.language == Some(ResourceLanguage::Rust))
            .map(|resource| resource.path_key)
            .collect();
        found.sort();
        found
    }

    fn symbols(&self, rel: &str) -> Vec<Symbol> {
        SymbolStore::open(&self.db_path)
            .expect("index.db")
            .list_for_resource(self.resource(rel).id)
            .expect("symbols")
    }

    /// The one symbol with this qualified name in one file.
    fn only(&self, rel: &str, qualified_name: &str) -> Symbol {
        let found: Vec<Symbol> = self
            .symbols(rel)
            .into_iter()
            .filter(|symbol| symbol.qualified_name == qualified_name)
            .collect();
        assert_eq!(
            found.len(),
            1,
            "{qualified_name} is declared once in {rel}, found {}",
            found.len()
        );
        found.into_iter().next().expect("one")
    }

    fn text(&self, rel: &str) -> String {
        fs::read_to_string(self.workspace.join(rel)).expect("source")
    }

    fn write(&self, rel: &str, contents: &str) {
        let path = self.workspace.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(path, contents).expect("write");
    }

    fn offset_of(&self, rel: &str, needle: &str, nth: usize) -> usize {
        let text = self.text(rel);
        text.match_indices(needle)
            .nth(nth)
            .unwrap_or_else(|| panic!("{needle:?} not in {rel}"))
            .0
    }

    /// Start a server, load the project under the slice's trust, and
    /// run `body` against it.
    fn with_host<T>(&self, body: impl FnOnce(&HostQueries<'_>) -> T) -> T {
        let _ = fs::remove_file(&self.marker);
        let host = self
            .launcher
            .start(&self.binding())
            .expect("server started");
        let queries = HostQueries {
            host: &host,
            cancel: CancelToken::new(),
        };
        // The initial load happens on its own; wait for the server to
        // say it has settled rather than for a clock.
        assert!(
            host.wait_for_quiescent(0, SETTLE_TIMEOUT),
            "the server must announce that it has settled"
        );
        // Hand the backend Brainprint's current bytes for every source.
        let handed_over: Vec<lifecycle::ResourceChange> = self
            .sources()
            .into_iter()
            .map(|rel| {
                lifecycle::ResourceChange::new(
                    self.resource(&rel).id,
                    lifecycle::ChangeKind::Changed,
                    rel,
                )
            })
            .collect();
        lifecycle::synchronize_documents(&queries, &queries, &self.workspace, &handed_over)
            .expect("documents handed over");

        let answer = body(&queries);
        self.assert_nothing_executed();
        SemanticRuntimeHost::shutdown(&host);
        answer
    }

    /// The trust assertion every live test makes.
    fn assert_nothing_executed(&self) {
        assert!(
            !self.marker.exists(),
            "the fixture's build.rs executed; the P0 configuration is not holding"
        );
        assert!(
            !self.workspace.join("target").exists(),
            "something was compiled: target/ appeared"
        );
    }

    fn refresh(
        &self,
        queries: &HostQueries<'_>,
        rel: &str,
    ) -> brainprint_engine::rust_semantic::RefreshOutcome {
        let index = SemanticIndex::open(&self.db_path).expect("index.db");
        let packages = self.packages();
        let config = packages.basis();
        let capabilities = capability_report(&self.context, self.trust);
        refresh_resource(
            &index,
            queries,
            &RefreshRequest {
                context: &self.context,
                workspace_root: &self.workspace,
                owner: self.resource(rel).id,
                config: &config,
                capabilities: &capabilities,
            },
        )
        .unwrap_or_else(|error| panic!("refresh {rel}: {error}"))
    }

    fn refresh_all(&self, queries: &HostQueries<'_>) {
        for rel in self.sources() {
            self.refresh(queries, &rel);
        }
    }

    /// Where one occurrence's relation points, read back through the
    /// ordinary graph API.
    fn target_at(&self, rel: &str, start: usize, end: usize) -> Option<GraphEndpoint> {
        self.relation_at(rel, start, end).map(|(target, _)| target)
    }

    fn relation_at(
        &self,
        rel: &str,
        start: usize,
        end: usize,
    ) -> Option<(GraphEndpoint, Dispatch)> {
        let owner = self.resource(rel).id;
        let index = RelationIndex::open(&self.db_path).expect("index.db");
        let symbols = self.symbols(rel);
        let mut sources: Vec<GraphEndpoint> = symbols
            .iter()
            .filter(|symbol| symbol.span.start_byte <= start && end <= symbol.span.end_byte)
            .map(|symbol| GraphEndpoint::Symbol(symbol.id))
            .collect();
        sources.push(GraphEndpoint::Resource(owner));
        for source in sources {
            let found = index
                .outgoing(
                    &source,
                    &[
                        RelationKind::Calls,
                        RelationKind::References,
                        RelationKind::Imports,
                        RelationKind::UsesType,
                        RelationKind::Implements,
                    ],
                )
                .expect("outgoing")
                .confirmed
                .into_iter()
                .find(|relation| {
                    relation.evidence.iter().any(|located| {
                        located.span.start_byte == start && located.span.end_byte == end
                    })
                })
                .map(|relation| (relation.target, relation.dispatch));
            if found.is_some() {
                return found;
            }
        }
        None
    }

    fn outgoing(&self, from: &GraphEndpoint, kinds: &[RelationKind]) -> Vec<GraphEndpoint> {
        RelationIndex::open(&self.db_path)
            .expect("index.db")
            .outgoing(from, kinds)
            .expect("outgoing")
            .confirmed
            .into_iter()
            .map(|relation| relation.target)
            .collect()
    }

    fn incoming(&self, into: &GraphEndpoint, kinds: &[RelationKind]) -> Vec<GraphEndpoint> {
        RelationIndex::open(&self.db_path)
            .expect("index.db")
            .incoming(into, kinds)
            .expect("incoming")
            .confirmed
            .into_iter()
            .map(|relation| relation.source)
            .collect()
    }

    fn name_of(&self, endpoint: &GraphEndpoint) -> String {
        let connection = Connection::open(&self.db_path).expect("index.db");
        match endpoint {
            GraphEndpoint::Symbol(symbol) => connection
                .query_row(
                    "SELECT resource.path_key || '::' || symbol.qualified_name \
                     || '@' || symbol.start_byte FROM symbol \
                     JOIN resource ON resource.id = symbol.resource_id WHERE symbol.uid = ?1",
                    rusqlite::params![symbol.to_bytes().to_vec()],
                    |row| row.get::<_, String>(0),
                )
                .unwrap_or_else(|_| format!("{symbol:?}")),
            GraphEndpoint::Resource(resource) => connection
                .query_row(
                    "SELECT 'module ' || path_key FROM resource WHERE uid = ?1",
                    rusqlite::params![resource.to_bytes().to_vec()],
                    |row| row.get::<_, String>(0),
                )
                .unwrap_or_else(|_| format!("{resource:?}")),
            other => format!("{other:?}"),
        }
    }

    fn names(&self, endpoints: &[GraphEndpoint]) -> Vec<String> {
        let mut found: Vec<String> = endpoints.iter().map(|one| self.name_of(one)).collect();
        found.sort();
        found.dedup();
        found
    }
}

impl Drop for Slice {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

/// The install, or the reason the suite cannot run.
///
/// The executable is discovered once, here, by the one toolchain
/// command a *test* may run. Production Brainprint is handed the path
/// and never asks rustup anything.
fn install_or_skip() -> Option<RustInstall> {
    let which = process::Command::new("rustup")
        .args(["which", "rust-analyzer"])
        .output()
        .ok()?;
    if !which.status.success() {
        eprintln!("skipping Rust acceptance: rust-analyzer is not installed");
        eprintln!("run scripts/rust_semantic_spike/install.sh first");
        return None;
    }
    let executable = String::from_utf8_lossy(&which.stdout).trim().to_owned();
    let install = RustInstall::at(&executable).ok()?;
    let rustc = process::Command::new("rustc")
        .args(["--version", "--verbose"])
        .output()
        .ok()?;
    let reported = String::from_utf8_lossy(&rustc.stdout).into_owned();
    let host_triple = reported
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap_or_default()
        .to_owned();
    let sysroot = process::Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    let sysroot = PathBuf::from(String::from_utf8_lossy(&sysroot.stdout).trim().to_owned());
    Some(install.with_toolchain(
        reported.lines().next().unwrap_or_default(),
        host_triple,
        &sysroot,
    ))
}

// ---------------------------------------------------------------------
// The whole-Workspace slice
// ---------------------------------------------------------------------

/// One pass over the whole fixture, read back through the ordinary
/// query APIs.
///
/// Everything asserted here travels `rust-analyzer → adapter → evidence
/// → publication → merge → query`. A probe that got the right answer
/// out of the backend proves nothing on its own; what matters is that
/// the canonical graph says it afterwards, to a caller who has never
/// heard of Rust.
#[test]
#[ignore = "needs an installed rust-analyzer"]
fn the_rust_slice_holds_against_the_real_backend() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("slice", 40, &install, ProjectExecutionTrust::Trusted);
    slice.with_host(|queries| {
        slice.refresh_all(queries);

        // ---- Associated function and inherent method ---------------
        let new = slice.only(RUNNER, "Worker::new");
        let execute = slice.only(RUNNER, "Worker::execute");
        let at = |needle: &str, name: &str| {
            let start = slice.offset_of(MAIN, needle, 0) + needle.find(name).expect("inside");
            (start, start + name.len())
        };

        let (start, end) = at("Worker::new(4)", "Worker::new");
        assert_eq!(
            slice.target_at(MAIN, start, end),
            Some(GraphEndpoint::Symbol(new.id)),
            "an associated function resolves to its own declaration"
        );
        let (start, end) = at("worker.execute()", "worker.execute");
        assert_eq!(
            slice.target_at(MAIN, start, end),
            Some(GraphEndpoint::Symbol(execute.id)),
            "and an inherent method to its own"
        );

        // ---- Two traits, one member name ---------------------------
        //
        // The trap this fixture exists for: `Worker` implements both
        // `Runner` and `Reporter`, both declare `run`, and both
        // implementing declarations are named `Worker::run`.
        let (start, end) = at("Runner::run(&worker)", "Runner::run");
        let as_runner = slice.target_at(MAIN, start, end);
        let (start, end) = at("Reporter::run(&worker)", "Reporter::run");
        let as_reporter = slice.target_at(MAIN, start, end);

        assert!(
            as_runner.is_some() && as_reporter.is_some(),
            "both resolved"
        );
        let runner_name = as_runner.as_ref().map(|one| slice.name_of(one));
        let reporter_name = as_reporter.as_ref().map(|one| slice.name_of(one));
        assert_ne!(
            as_runner, as_reporter,
            "two traits declaring the same member name are two declarations: \
             {runner_name:?} vs {reporter_name:?}"
        );
        // Fully qualified syntax picks the same one as the trait call.
        let (start, end) = at(
            "<Worker as Runner>::run(&worker)",
            "<Worker as Runner>::run",
        );
        assert_eq!(
            slice.target_at(MAIN, start, end),
            as_runner,
            "UFCS names the same declaration the trait call does"
        );

        // A same-name inherent method on a type implementing nothing.
        let idle_run = slice.only(RUNNER, "Idle::run");
        let (start, end) = at("Idle.run()", "Idle.run");
        assert_eq!(
            slice.target_at(MAIN, start, end),
            Some(GraphEndpoint::Symbol(idle_run.id)),
            "an inherent method on an unrelated type is its own declaration"
        );

        // ---- Dispatch honesty ---------------------------------------
        //
        // Measured: a concrete call resolves into an `impl`, a `dyn`
        // call into the `trait`. So the two are told apart by where the
        // compiler pointed.
        let dyn_site = slice.offset_of(RUNNER, "value.run()", 2);
        if let Some((target, dispatch)) =
            slice.relation_at(RUNNER, dyn_site, dyn_site + "value.run".len())
        {
            let named = slice.name_of(&target);
            assert_eq!(
                dispatch,
                Dispatch::Dynamic,
                "a call through &dyn Runner is not statically bound: it resolved to {named}"
            );
        }
        let (start, end) = at("worker.execute()", "worker.execute");
        assert_eq!(
            slice.relation_at(MAIN, start, end).map(|(_, kind)| kind),
            Some(Dispatch::Static),
            "an inherent method call is bound exactly where it points"
        );

        // ---- Re-exports ---------------------------------------------
        let model = slice.only(CONTRACTS, "Model");
        let worker = slice.only(RUNNER, "Worker");
        let (start, end) = at("PublicModel::new(3)", "PublicModel::new");
        let through_reexport = slice.target_at(MAIN, start, end);
        assert!(
            through_reexport.is_some(),
            "a named re-export reaches the real declaration"
        );
        let (start, end) = at("PublicWorker::new(6)", "PublicWorker::new");
        assert!(
            slice.target_at(MAIN, start, end).is_some(),
            "and so does an aliased one"
        );
        let _ = (model, worker);

        // ---- Generic type and function -------------------------------
        let identity = slice.only("crates/core/src/model.rs", "identity");
        let (start, end) = at("identity(boxed.value)", "identity");
        assert_eq!(
            slice.target_at(MAIN, start, end),
            Some(GraphEndpoint::Symbol(identity.id)),
            "a generic function resolves to its declaration"
        );
        let boxed_new = slice.only("crates/core/src/model.rs", "Boxed::new");
        let (start, end) = at("Boxed::new(9_u32)", "Boxed::new");
        assert_eq!(
            slice.target_at(MAIN, start, end),
            Some(GraphEndpoint::Symbol(boxed_new.id)),
            "and an associated function on a generic type to its own"
        );

        // ---- Trait implementation ------------------------------------
        let runner_trait = slice.only(CONTRACTS, "Runner");
        let worker_struct = slice.only(RUNNER, "Worker");
        let other_struct = slice.only(RUNNER, "Other");
        let implements = slice.outgoing(
            &GraphEndpoint::Symbol(worker_struct.id),
            &[RelationKind::Implements],
        );
        assert!(
            implements.contains(&GraphEndpoint::Symbol(runner_trait.id)),
            "Worker IMPLEMENTS Runner, sourced from the type and not the file: {:?}",
            slice.names(&implements)
        );
        // Two implementers, so the query returns a set.
        let implementers = slice.incoming(
            &GraphEndpoint::Symbol(runner_trait.id),
            &[RelationKind::Implements],
        );
        let found = slice.names(&implementers);
        assert!(
            found.iter().any(|name| name.contains("Worker")),
            "{found:?}"
        );
        assert!(found.iter().any(|name| name.contains("Other")), "{found:?}");
        let _ = other_struct;

        // An inherent impl creates none of this.
        let idle = slice.only(RUNNER, "Idle");
        assert!(
            slice
                .outgoing(&GraphEndpoint::Symbol(idle.id), &[RelationKind::Implements])
                .is_empty(),
            "an inherent impl implements nothing"
        );

        // ---- Rust has no inheritance or overriding -------------------
        for (rel, name) in [(RUNNER, "Worker"), (CONTRACTS, "Detailed")] {
            let symbol = slice.only(rel, name);
            assert!(
                slice
                    .outgoing(&GraphEndpoint::Symbol(symbol.id), &[RelationKind::Extends])
                    .is_empty(),
                "{name} extends nothing: Rust has no class inheritance"
            );
            assert!(
                slice
                    .outgoing(
                        &GraphEndpoint::Symbol(symbol.id),
                        &[RelationKind::Overrides]
                    )
                    .is_empty(),
                "{name} overrides nothing"
            );
        }

        // ---- Nothing generated became source -------------------------
        assert!(
            slice
                .resources()
                .iter()
                .all(|resource| !resource.path_key.contains("target/")),
            "build output is never a Resource"
        );
        assert!(
            slice.resources().iter().all(|resource| {
                !resource.path_key.contains(".cargo") && !resource.path_key.contains(".rustup")
            }),
            "no dependency or toolchain tree is indexed"
        );
    });
}

// ---------------------------------------------------------------------
// Trust and execution
// ---------------------------------------------------------------------

/// The same Workspace, untrusted: no manifest is re-read, nothing is
/// executed, and nothing cross-crate is claimed.
#[test]
#[ignore = "needs an installed rust-analyzer"]
fn an_untrusted_workspace_executes_nothing_and_claims_nothing() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("untrusted", 41, &install, ProjectExecutionTrust::Untrusted);

    // The gate is in the host, so even a direct call is refused.
    let host = slice.launcher.start(&slice.binding()).expect("server");
    let refused = host.call(&RustRequest::ReloadWorkspace, &CancelToken::new());
    assert!(refused.is_err(), "an untrusted host must refuse to reload");
    SemanticRuntimeHost::shutdown(&host);

    slice.assert_nothing_executed();
    let capabilities = capability_report(&slice.context, ProjectExecutionTrust::Untrusted);
    assert_eq!(
        capabilities.support(SemanticCapability::Implements),
        brainprint_engine::resolution::Support::Unsupported
    );
    assert_eq!(
        capabilities.support(SemanticCapability::SyntaxStructure),
        brainprint_engine::resolution::Support::Supported
    );
}

/// A trusted load reads manifests and runs nothing.
///
/// The marker `build.rs` writes a file if it is ever executed, and
/// `target/` appears if anything is compiled. Both are asserted absent
/// after a full load and a full refresh.
#[test]
#[ignore = "needs an installed rust-analyzer"]
fn a_trusted_load_runs_no_build_script_and_compiles_nothing() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("no-exec", 42, &install, ProjectExecutionTrust::Trusted);
    slice.with_host(|queries| {
        slice.refresh_all(queries);
        // `with_host` asserts it too, on the way out. Asserted here as
        // well because this test exists for exactly this.
        slice.assert_nothing_executed();
        assert!(
            !slice.workspace.join("crates/core/build.rs.ran").exists(),
            "the build script left nothing behind"
        );
    });
}

// ---------------------------------------------------------------------
// Freshness
// ---------------------------------------------------------------------

/// A source edit becomes current after the document barrier, with no
/// reload and no sleep.
#[test]
#[ignore = "needs an installed rust-analyzer"]
fn a_source_edit_is_current_after_the_document_barrier() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("edit", 43, &install, ProjectExecutionTrust::Trusted);
    slice.with_host(|queries| {
        slice.refresh_all(queries);
        let before = slice.only("crates/core/src/model.rs", "identity");

        // Two lines above it: every declaration below moves.
        let original = slice.text("crates/core/src/model.rs");
        slice.write(
            "crates/core/src/model.rs",
            &format!("// shifted\n// shifted\n{original}"),
        );
        slice.withdraw_all();
        slice.rescan("workspace-rev-2");

        let change = lifecycle::ResourceChange::new(
            slice.resource("crates/core/src/model.rs").id,
            lifecycle::ChangeKind::Changed,
            "crates/core/src/model.rs",
        );
        assert_eq!(
            change.class(),
            lifecycle::ChangeClass::DocumentContent,
            "editing a body is a source change"
        );
        lifecycle::synchronize_documents(
            queries,
            queries,
            &slice.workspace,
            std::slice::from_ref(&change),
        )
        .expect("document sync");

        slice.refresh(queries, MAIN);
        let moved = slice.only("crates/core/src/model.rs", "identity");
        assert_ne!(before.span.start_byte, moved.span.start_byte, "it moved");

        let start = slice.offset_of(MAIN, "identity(boxed.value)", 0);
        assert_eq!(
            slice.target_at(MAIN, start, start + "identity".len()),
            Some(GraphEndpoint::Symbol(moved.id)),
            "the answer follows the declaration to its new line"
        );
    });
}

/// A new module becomes current through the document sync, because in
/// Rust the module tree is source.
///
/// `src/extra.rs` is not a module until `lib.rs` says `mod extra;`, and
/// saying it is an edit. Adding the file reloads nothing.
#[test]
#[ignore = "needs an installed rust-analyzer"]
fn a_new_module_needs_no_project_reload() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("new-module", 44, &install, ProjectExecutionTrust::Trusted);
    slice.with_host(|queries| {
        slice.refresh_all(queries);

        slice.write(
            "crates/core/src/extra.rs",
            "//! A module that did not exist a moment ago.\n\npub fn added() -> u32 {\n    11\n}\n",
        );
        let lib = slice.text("crates/core/src/lib.rs");
        slice.write(
            "crates/core/src/lib.rs",
            &lib.replace("pub mod model;", "pub mod extra;\npub mod model;"),
        );
        slice.withdraw_all();
        slice.rescan("workspace-rev-2");

        let changes: Vec<lifecycle::ResourceChange> = [
            ("crates/core/src/extra.rs", lifecycle::ChangeKind::Added),
            ("crates/core/src/lib.rs", lifecycle::ChangeKind::Changed),
        ]
        .into_iter()
        .map(|(rel, kind)| lifecycle::ResourceChange::new(slice.resource(rel).id, kind, rel))
        .collect();
        for change in &changes {
            assert_eq!(
                change.class(),
                lifecycle::ChangeClass::DocumentContent,
                "a Rust module tree change is source, not a manifest change"
            );
        }
        lifecycle::synchronize_documents(queries, queries, &slice.workspace, &changes)
            .expect("document sync");

        // The new module's own declaration is visible to the backend,
        // which is what a reload would otherwise have been needed for.
        let structure = queries
            .call(&RustRequest::DocumentSymbol {
                uri: path_to_uri(&slice.workspace.join("crates/core/src/extra.rs")),
            })
            .expect("document symbols");
        let RustResponse::DocumentSymbols(members) = structure else {
            panic!("document symbols");
        };
        assert!(
            members.iter().any(|member| member.name.contains("added")),
            "the new module is known: {members:?}"
        );
        slice.assert_nothing_executed();
    });
}

/// A manifest change reloads the project and waits for the barrier.
#[test]
#[ignore = "needs an installed rust-analyzer"]
fn a_manifest_change_reloads_and_waits_for_quiescence() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("reload", 45, &install, ProjectExecutionTrust::Trusted);
    slice.with_host(|queries| {
        let manifest = slice.text("crates/core/Cargo.toml");
        slice.write(
            "crates/core/Cargo.toml",
            &manifest.replace("extra = []", "extra = []\nspare = []"),
        );
        slice.rescan("workspace-rev-2");

        let change = lifecycle::ResourceChange::new(
            slice.resource("crates/core/Cargo.toml").id,
            lifecycle::ChangeKind::Changed,
            "crates/core/Cargo.toml",
        )
        .with_language(None);
        assert_eq!(
            change.class(),
            lifecycle::ChangeClass::ProjectDefinition,
            "a manifest is the project's own definition"
        );

        let settled = lifecycle::reload_projects(
            queries,
            queries,
            &slice.workspace,
            std::slice::from_ref(&change),
            &slice.packages(),
        )
        .expect("the barrier fires");
        assert!(settled >= 1, "the server settled after the reload");
        slice.assert_nothing_executed();
    });
}

// ---------------------------------------------------------------------
// Agent-facing surfaces
// ---------------------------------------------------------------------

/// Impact, related tests and prepared source, through the unchanged
/// common APIs.
#[test]
#[ignore = "needs an installed rust-analyzer"]
fn the_agent_surfaces_answer_for_rust() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("surfaces", 46, &install, ProjectExecutionTrust::Trusted);
    slice.with_host(|queries| {
        slice.refresh_all(queries);

        let runner_trait = slice.only(CONTRACTS, "Runner");
        let worker = slice.only(RUNNER, "Worker");
        let impact = ImpactTraversal::open(&slice.db_path)
            .expect("index.db")
            .run(
                ImpactIntent::BaseInterfaceChange,
                &GraphEndpoint::Symbol(runner_trait.id),
                &Budget::default(),
            )
            .expect("impact");
        assert!(
            impact
                .nodes
                .iter()
                .any(|node| node.endpoint == GraphEndpoint::Symbol(worker.id)),
            "a trait change reaches the types that implement it"
        );

        let new = slice.only(RUNNER, "Worker::new");
        let related = RelatedTests::open(&slice.db_path)
            .expect("index.db")
            .for_target(
                &GraphEndpoint::Symbol(new.id),
                ImpactIntent::PublicSignatureChange,
                &Budget::default(),
            )
            .expect("related tests");
        assert!(
            related
                .candidates
                .iter()
                .any(|candidate| candidate.path_rel.contains("integration")
                    || candidate.path_rel.contains("lib.rs")),
            "the tests that exercise it are reachable: {:?}",
            related
                .candidates
                .iter()
                .map(|candidate| &candidate.path_rel)
                .collect::<Vec<_>>()
        );

        // Prepared inspection: current source, not a line number.
        let prepared = InspectPreparer::open(&slice.db_path, &slice.workspace)
            .expect("preparer")
            .prepare(
                &GraphEndpoint::Symbol(runner_trait.id),
                Direction::Incoming,
                &[RelationKind::Implements, RelationKind::References],
            )
            .expect("prepared");
        assert!(prepared.confirmed_count() > 0);
        assert!(
            prepared.source_complete(),
            "every prepared range carries verified current source"
        );
        for relation in &prepared.relations {
            for item in &relation.evidence {
                assert!(
                    item.evidence_range.is_some(),
                    "a semantic result with only a location: {:?}",
                    item.location
                );
                assert!(item.unavailable.is_none(), "{:?}", item.unavailable);
            }
        }
        // And nothing generated or virtual reached an Agent.
        for range in &prepared.ranges {
            assert!(
                !range.source.contains("macro-expansion"),
                "prepared source is editable Workspace source"
            );
        }
    });
}

// ---------------------------------------------------------------------
// The shared runtime
// ---------------------------------------------------------------------

/// Two callers, one rust-analyzer.
#[test]
#[ignore = "needs an installed rust-analyzer"]
fn one_analysis_context_runs_one_rust_analyzer() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open(
        "shared-runtime",
        47,
        &install,
        ProjectExecutionTrust::Trusted,
    );
    let supervisor = SemanticRuntimeSupervisor::new(RuntimePolicy::default())
        .with_backend(Arc::clone(&slice.launcher) as Arc<dyn SemanticBackendLauncher>);
    let first = supervisor.acquire(&slice.binding()).expect("first caller");
    let second = supervisor.acquire(&slice.binding()).expect("second caller");
    assert_eq!(
        supervisor.live_runtime_count(),
        1,
        "two callers, one crate graph"
    );
    drop((first, second));
    supervisor.shutdown();
}

/// A restarted backend holds nothing from the old connection.
#[test]
#[ignore = "needs an installed rust-analyzer"]
fn a_restarted_backend_reopens_rather_than_changes() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("restart", 48, &install, ProjectExecutionTrust::Trusted);
    let uri = path_to_uri(&slice.workspace.join(RUNNER));

    let first = slice
        .launcher
        .start(&slice.binding())
        .expect("first server");
    assert_eq!(
        first.exchange_text(&uri, "fn a() {}"),
        None,
        "a cold server holds nothing"
    );
    let first_version = first.next_document_version();
    SemanticRuntimeHost::shutdown(&first);

    let second = slice.launcher.start(&slice.binding()).expect("restarted");
    assert_eq!(
        second.exchange_text(&uri, "fn a() {}"),
        None,
        "a restarted server has opened nothing, whatever the old one had"
    );
    assert_eq!(
        second.next_document_version(),
        first_version,
        "and its version sequence starts over with it"
    );
    SemanticRuntimeHost::shutdown(&second);
}
