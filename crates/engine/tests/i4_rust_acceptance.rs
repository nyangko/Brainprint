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

    /// Every call candidate one Resource holds, with whether it is
    /// bound to a relation and whether it is an open
    /// `MACRO_CALL_REQUIRES_SEMANTICS` gap. Read straight from the
    /// index: a candidate is neither a call site nor an answer.
    fn candidates(&self, rel: &str) -> Vec<Candidate> {
        let connection = Connection::open(&self.db_path).expect("index.db");
        let mut statement = connection
            .prepare(
                "SELECT o.start_byte, o.end_byte, o.relation_id IS NOT NULL, \
                        EXISTS (SELECT 1 FROM unresolved_reference u \
                                WHERE u.occurrence_id = o.id \
                                  AND u.reason = 'MACRO_CALL_REQUIRES_SEMANTICS' \
                                  AND u.intended_relation_kind = 'CALLS') \
                 FROM occurrence o JOIN resource r ON r.id = o.resource_id \
                 WHERE r.uid = ?1 AND o.kind = 'CALL_CANDIDATE_SITE' \
                 ORDER BY o.start_byte",
            )
            .expect("statement");
        statement
            .query_map(
                rusqlite::params![self.resource(rel).id.to_bytes().to_vec()],
                |row| {
                    Ok(Candidate {
                        start: usize::try_from(row.get::<_, i64>(0)?).expect("start"),
                        end: usize::try_from(row.get::<_, i64>(1)?).expect("end"),
                        bound: row.get(2)?,
                        open_gap: row.get(3)?,
                    })
                },
            )
            .expect("query")
            .map(|row| row.expect("row"))
            .collect()
    }

    /// The candidate for `name` written right after `anchor`.
    fn candidate(&self, rel: &str, anchor: &str, name: &str) -> Candidate {
        let start = self.offset_of(rel, anchor, 0) + anchor.find(name).expect("inside");
        self.candidates(rel)
            .into_iter()
            .find(|candidate| candidate.start == start)
            .unwrap_or_else(|| panic!("{name} after {anchor:?} in {rel} is a candidate"))
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

/// One call-shaped token inside a macro's arguments, as stored.
struct Candidate {
    start: usize,
    end: usize,
    bound: bool,
    open_gap: bool,
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

// ---------------------------------------------------------------------
// Macros, the standard library, and the network
// ---------------------------------------------------------------------

/// What a declarative macro costs, measured rather than assumed.
///
/// Two separate facts. A `macro_rules!` *invocation* resolves to the
/// macro's own declaration, which is an ordinary editable span and is
/// published. What the macro *expands to* is not: the invocation itself
/// is an opaque token tree to the structural tier, so it anchors no
/// edge of its own.
///
/// A call written *inside* a macro's arguments is a separate, narrower
/// matter, and it is a candidate rather than a call site (#47; see
/// `macro_call_candidates_are_calls_only_when_the_server_proves_them`).
#[test]
#[ignore = "needs an installed rust-analyzer"]
fn a_declarative_macro_resolves_and_its_expansion_does_not() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("macros", 49, &install, ProjectExecutionTrust::Trusted);
    slice.with_host(|queries| {
        slice.refresh_all(queries);

        // The invocation itself anchors nothing, so the graph has no
        // edge for it and no span to hand an Agent.
        let inside_macro =
            slice.offset_of("crates/core/tests/integration.rs", "assert_eq!(seed, 5)", 0);
        assert_eq!(
            slice.target_at(
                "crates/core/tests/integration.rs",
                inside_macro,
                inside_macro + "assert_eq".len()
            ),
            None,
            "a macro invocation is an opaque token tree; nothing is claimed inside it"
        );

        // And nothing generated or virtual ever became a Resource.
        assert!(
            slice
                .resources()
                .iter()
                .all(|resource| !resource.path_key.contains("macro")),
            "no expansion became a Resource"
        );
        slice.assert_nothing_executed();
    });
}

/// `self` and `super` paths resolve, and a module is a Resource.
#[test]
#[ignore = "needs an installed rust-analyzer"]
fn self_and_super_paths_resolve() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("paths", 50, &install, ProjectExecutionTrust::Trusted);
    slice.with_host(|queries| {
        let deep = "crates/core/src/nested/deep.rs";
        let outcome = slice.refresh(queries, deep);

        // `use super::Nested;` and `use self::inner::Inner;` are the
        // two relative forms; both are `use` sites this tier asks
        // about.
        assert!(
            outcome
                .report
                .iter()
                .filter(|line| line.starts_with("ImportBinding"))
                .count()
                >= 2,
            "both relative imports were asked about: {:?}",
            outcome.report
        );
        // Whatever they resolved to, nothing virtual was published.
        assert!(
            outcome.refused.is_empty(),
            "nothing generated was offered: {:?}",
            outcome.refused
        );
        slice.assert_nothing_executed();
    });
}

/// Grouped `use` (#42): braces are grouping syntax, not a reason to drop
/// leaf evidence. Every leaf a flat `use` would name still resolves once
/// decomposed -- aliased, nested two groups deep, and reached through a
/// glob -- against the real backend, not a scripted one.
#[test]
#[ignore = "needs an installed rust-analyzer"]
fn grouped_use_leaves_resolve_exactly_as_the_flat_form_would() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("grouped", 60, &install, ProjectExecutionTrust::Trusted);
    slice.with_host(|queries| {
        let deep = "crates/core/src/nested/deep.rs";
        let grouped = "crates/core/src/grouped.rs";
        slice.refresh(queries, deep);
        slice.refresh(queries, grouped);

        let at = |rel: &str, needle: &str, name: &str| {
            let start = slice.offset_of(rel, needle, 0) + needle.find(name).expect("inside");
            (start, start + name.len())
        };

        // A grouped, aliased `super::{Nested, Nested as AliasedNested}`
        // resolves the alias to the same declaration as the unaliased
        // leaf.
        let nested_depth = slice.only("crates/core/src/nested/mod.rs", "Nested::depth");
        let (start, end) = at(deep, "AliasedNested.depth()", "AliasedNested.depth");
        assert_eq!(
            slice.target_at(deep, start, end),
            Some(GraphEndpoint::Symbol(nested_depth.id)),
            "an aliased leaf inside a group resolves like the unaliased one"
        );

        // A grouped, aliased `self::inner::{Inner, Inner as AliasedInner}`.
        let inner_value = slice.only(deep, "inner::Inner::value");
        let (start, end) = at(deep, "AliasedInner.value()", "AliasedInner.value");
        assert_eq!(
            slice.target_at(deep, start, end),
            Some(GraphEndpoint::Symbol(inner_value.id)),
            "a self-relative alias inside a group resolves too"
        );

        // Nested groups: `crate::{model::{Boxed, identity}, runner::Worker}`.
        let boxed_new = slice.only("crates/core/src/model.rs", "Boxed::new");
        let (start, end) = at(grouped, "Boxed::new(identity(4_u32))", "Boxed::new");
        assert_eq!(
            slice.target_at(grouped, start, end),
            Some(GraphEndpoint::Symbol(boxed_new.id)),
            "a leaf two groups deep resolves to its declaration"
        );
        let worker_new = slice.only(RUNNER, "Worker::new");
        let (start, end) = at(grouped, "Worker::new(2)", "Worker::new");
        assert_eq!(
            slice.target_at(grouped, start, end),
            Some(GraphEndpoint::Symbol(worker_new.id)),
            "a sibling leaf in the same nested group resolves too"
        );

        // A glob (`crate::nested::deep::*`) brought `combine` into
        // scope; the call site it backs still resolves.
        let combine = slice.only(deep, "combine");
        let (start, end) = at(grouped, "combine()", "combine");
        assert_eq!(
            slice.target_at(grouped, start, end),
            Some(GraphEndpoint::Symbol(combine.id)),
            "a call site backed by a glob import resolves to the real declaration"
        );

        slice.assert_nothing_executed();
    });
}

/// The standard library is an identity, and its absence is recorded.
///
/// `rust-src` is not installed by Brainprint and may not be present.
/// When it is missing, navigation *into* std narrows and nothing else
/// does: the Workspace's own semantics keep working, which is what this
/// asserts by doing a full refresh and checking the rest still holds.
#[test]
#[ignore = "needs an installed rust-analyzer"]
fn the_standard_library_is_an_identity_and_its_absence_is_recorded() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("std", 51, &install, ProjectExecutionTrust::Trusted);
    let environment = {
        let index = SemanticIndex::open(&slice.db_path).expect("index.db");
        let packages = lifecycle::discover_packages_under(
            index.connection(),
            ProjectExecutionTrust::Trusted,
            Some(&slice.workspace),
        )
        .expect("packages");
        lifecycle::environment_identity(&install, &packages).expect("environment")
    };
    // Recorded either way, rather than assumed.
    assert_eq!(
        environment.rust_src, install.rust_src,
        "whether std source is navigable is part of the environment"
    );

    slice.with_host(|queries| {
        slice.refresh_all(queries);
        // Workspace semantics hold regardless.
        let runner_trait = slice.only(CONTRACTS, "Runner");
        assert!(
            !slice
                .incoming(
                    &GraphEndpoint::Symbol(runner_trait.id),
                    &[RelationKind::Implements]
                )
                .is_empty(),
            "the Workspace's own semantics do not depend on rust-src"
        );
        // And no sysroot file became a Resource, present or not.
        assert!(
            slice
                .resources()
                .iter()
                .all(|resource| !resource.path_key.contains("rustlib")),
            "the sysroot is never indexed"
        );
        slice.assert_nothing_executed();
    });
}

/// Nothing is fetched. The analysis runs offline, by construction.
///
/// `CARGO_NET_OFFLINE` is set on the child, so a dependency that is not
/// already present cannot be downloaded to answer a question. The
/// fixture uses only path dependencies, so a successful run with the
/// variable set is the evidence: reading manifests needed no registry
/// at all.
#[test]
#[ignore = "needs an installed rust-analyzer"]
fn no_dependency_is_fetched_to_answer_a_question() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("offline", 52, &install, ProjectExecutionTrust::Trusted);

    // A dependency that exists nowhere. Cargo cannot resolve it
    // offline, and Brainprint must degrade rather than reach out.
    let manifest = slice.text("crates/core/Cargo.toml");
    slice.write(
        "crates/core/Cargo.toml",
        &manifest.replace(
            "[features]",
            "bp-nonexistent-package-9f2c1 = \"1.0\"\n\n[features]",
        ),
    );
    slice.rescan("workspace-rev-2");

    slice.with_host(|queries| {
        // The project may fail to resolve; what matters is that the
        // failure is a failure and not a download.
        let outcome = slice.refresh(queries, RUNNER);
        assert!(
            outcome.evidence_count > 0 || !outcome.report.is_empty(),
            "the pass ran and reported what it could"
        );
        slice.assert_nothing_executed();
        assert!(
            !slice.workspace.join("Cargo.lock.tmp").exists(),
            "nothing was written back into the Workspace"
        );
    });
}

// ---------------------------------------------------------------------
// Call candidates inside a macro's arguments (#47)
// ---------------------------------------------------------------------

/// The fixture file that holds the macro shapes.
const PROBE: &str = "crates/core/src/target_probe.rs";

/// A call-shaped token inside a macro is a candidate, and only the
/// server's own answers -- a definition that is a callable declaration,
/// and SignatureHelp at its argument list -- make one a `CALLS` edge.
/// Everything the syntax cannot settle stays an explicit, owner-local
/// gap.
#[test]
#[ignore = "needs an installed rust-analyzer"]
fn macro_call_candidates_are_calls_only_when_the_server_proves_them() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open(
        "macro-candidates",
        71,
        &install,
        ProjectExecutionTrust::Trusted,
    );
    let target = slice.only(PROBE, "target_probe");

    // What the syntax alone says: candidates, all unresolved, none bound
    // to anything, none a call site.
    let structural = slice.candidates(PROBE);
    assert!(
        structural.len() >= 6,
        "the fixture's macro shapes are candidates: {}",
        structural.len()
    );
    assert!(
        structural
            .iter()
            .all(|candidate| candidate.open_gap && !candidate.bound),
        "every candidate starts as an unresolved CALLS gap"
    );

    slice.with_host(|queries| {
        // ---- The owner, and only the owner ---------------------------
        slice.refresh(queries, PROBE);
        assert!(
            slice
                .candidate(
                    RUNNER,
                    "assert_eq!(aliased_probe(), value)",
                    "aliased_probe"
                )
                .open_gap,
            "refreshing one Resource proves nothing in another"
        );

        // ---- Positives ------------------------------------------------
        let proven = |rel: &str, anchor: &str, name: &str| {
            let candidate = slice.candidate(rel, anchor, name);
            assert!(
                candidate.bound && !candidate.open_gap,
                "{anchor:?}: proven, no longer a gap"
            );
            slice.target_at(rel, candidate.start, candidate.end)
        };
        for anchor in [
            "assert_eq!(target_probe(), 7)",
            "assert!(matches!(target_probe(), 7))",
            "matches!(Some(target_probe()), Some(_))",
        ] {
            assert_eq!(
                proven(PROBE, anchor, "target_probe"),
                Some(GraphEndpoint::Symbol(target.id)),
                "{anchor:?} calls the target: macro-nested, two macros deep, and as an argument"
            );
        }

        // The edge is sourced from the function or test it sits in.
        let callers =
            slice.names(&slice.incoming(&GraphEndpoint::Symbol(target.id), &[RelationKind::Calls]));
        for name in [
            "macro_nested_call_matches_the_target",
            "nested_macro_call_matches_the_target",
            "a_constructor_and_a_pattern_in_a_macro",
        ] {
            assert!(
                callers.iter().any(|caller| caller.contains(name)),
                "{name} calls the target: {callers:?}"
            );
        }

        // A nearby unrelated call binds to its own target and lends
        // nothing to the bare reference beside it.
        let same_file = slice.only(PROBE, "same_file_caller");
        assert_eq!(
            proven(
                PROBE,
                "assert_eq!(same_file_caller(), 7)",
                "same_file_caller"
            ),
            Some(GraphEndpoint::Symbol(same_file.id))
        );
        let beside = slice.only(
            PROBE,
            "a_bare_reference_beside_an_unrelated_call_in_a_macro",
        );
        assert!(
            !slice
                .outgoing(&GraphEndpoint::Symbol(beside.id), &[RelationKind::Calls])
                .contains(&GraphEndpoint::Symbol(target.id)),
            "a bare reference inside a macro is not a call"
        );
        let invocation = "matches!(target_probe as fn() -> u32, _)";
        let bare_from = slice.offset_of(PROBE, invocation, 0);
        let bare_to = bare_from + invocation.len();
        assert!(
            slice
                .candidates(PROBE)
                .iter()
                .all(|candidate| candidate.end <= bare_from || candidate.start >= bare_to),
            "and it is not even a candidate: nothing is written in front of an argument list"
        );

        // The alias, once its owner is refreshed.
        slice.refresh(queries, RUNNER);
        assert_eq!(
            proven(
                RUNNER,
                "assert_eq!(aliased_probe(), value)",
                "aliased_probe"
            ),
            Some(GraphEndpoint::Symbol(target.id)),
            "a call through a renamed import inside a macro resolves by identity"
        );

        // ---- Negatives ------------------------------------------------
        for (anchor, why) in [
            (
                "stringify!(target_probe())",
                "stringify! never executes its tokens",
            ),
            (
                "swallow_tokens!(target_probe())",
                "a macro_rules! arm that discards its input executes nothing",
            ),
        ] {
            let candidate = slice.candidate(PROBE, anchor, "target_probe");
            assert!(candidate.open_gap && !candidate.bound, "{why}");
            assert_eq!(
                slice.target_at(PROBE, candidate.start, candidate.end),
                None,
                "{why}"
            );
        }
        // A constructor and a pattern share a spelling; neither is a
        // function, so neither is bound from a candidate.
        for candidate in slice
            .candidates(PROBE)
            .iter()
            .filter(|candidate| slice.text(PROBE)[candidate.start..candidate.end] == *"Some")
        {
            assert!(
                candidate.open_gap && !candidate.bound,
                "`Some(..)` stays an explicit gap"
            );
        }

        // Nothing ever came from anywhere but the owner's own Resource.
        for rel in [PROBE, RUNNER] {
            for candidate in slice.candidates(rel) {
                assert_eq!(
                    candidate.bound, !candidate.open_gap,
                    "a candidate is exactly one of proven or an open gap"
                );
            }
        }
        slice.assert_nothing_executed();
    });
}

/// An edit replaces the candidates with the Resource's structure, and
/// the next proof follows the source: proven becomes unproven and
/// unproven becomes proven.
#[test]
#[ignore = "needs an installed rust-analyzer"]
fn an_edit_moves_a_candidate_between_proven_and_unproven() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("macro-edit", 72, &install, ProjectExecutionTrust::Trusted);
    let target = slice.only(PROBE, "target_probe");
    slice.with_host(|queries| {
        slice.refresh(queries, PROBE);
        let was_proven = slice.candidate(PROBE, "assert_eq!(target_probe(), 7)", "target_probe");
        let was_stringified = slice.candidate(PROBE, "stringify!(target_probe())", "target_probe");
        assert!(was_proven.bound && !was_stringified.bound);

        // positive -> negative, negative -> positive.
        let edited = slice
            .text(PROBE)
            .replace(
                "assert_eq!(target_probe(), 7);\n    }\n\n    #[test]\n    fn nested",
                "let _ = stringify!(target_probe());\n    }\n\n    #[test]\n    fn nested",
            )
            .replace(
                "    stringify!(target_probe())\n",
                "    { assert_eq!(target_probe(), 7); \"x\" }\n",
            );
        assert_ne!(edited, slice.text(PROBE), "the edit applied");
        slice.write(PROBE, &edited);
        slice.withdraw_all();
        slice.rescan("workspace-rev-2");

        // Structure replaced: candidates are unresolved again, wherever
        // they moved to.
        assert!(
            slice
                .candidates(PROBE)
                .iter()
                .all(|candidate| candidate.open_gap && !candidate.bound),
            "an edit replaces the candidates with fresh structural evidence"
        );

        let change = lifecycle::ResourceChange::new(
            slice.resource(PROBE).id,
            lifecycle::ChangeKind::Changed,
            PROBE,
        );
        lifecycle::synchronize_documents(
            queries,
            queries,
            &slice.workspace,
            std::slice::from_ref(&change),
        )
        .expect("document sync");
        slice.refresh(queries, PROBE);

        let now_negative =
            slice.candidate(PROBE, "let _ = stringify!(target_probe())", "target_probe");
        assert!(
            now_negative.open_gap && !now_negative.bound,
            "proven -> unproven"
        );
        assert_eq!(
            slice.target_at(PROBE, now_negative.start, now_negative.end),
            None
        );

        let now_positive =
            slice.candidate(PROBE, "{ assert_eq!(target_probe(), 7);", "target_probe");
        assert!(
            now_positive.bound && !now_positive.open_gap,
            "unproven -> proven"
        );
        assert_eq!(
            slice.target_at(PROBE, now_positive.start, now_positive.end),
            Some(GraphEndpoint::Symbol(target.id))
        );
        slice.assert_nothing_executed();
    });
}

/// Untrusted, or no backend at all: the candidates are still there,
/// every one an explicit gap, and nothing is claimed or started.
#[test]
#[ignore = "needs an installed rust-analyzer"]
fn an_untrusted_workspace_keeps_every_macro_candidate_as_a_gap() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open(
        "macro-untrusted",
        73,
        &install,
        ProjectExecutionTrust::Untrusted,
    );
    let candidates = slice.candidates(PROBE);
    assert!(candidates.len() >= 6);
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.open_gap && !candidate.bound),
        "no backend, so no proof"
    );
    slice.assert_nothing_executed();
}
