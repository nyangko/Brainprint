//! C# semantic acceptance, against the real Roslyn language server
//! (#19 task 12).
//!
//! What this proves that the scripted tests cannot: that the answers the
//! adapter is built around are the answers
//! `Microsoft.CodeAnalysis.LanguageServer` actually gives, over a real
//! multi-project solution, driven through Brainprint's own launcher,
//! host and protocol code.
//!
//! Every fact travels the whole tier and is read back through the
//! ordinary APIs:
//!
//! ```text
//! .cs → LSP → adapter → evidence → publication → merge → query API
//! ```
//!
//! ```sh
//! cd scripts/csharp_semantic_spike && ./restore.sh && cd -
//! cargo test -p brainprint-engine --test i4_csharp_acceptance -- --ignored --nocapture
//! ```
//!
//! ## Why these tests load projects
//!
//! Loading a project runs MSBuild, which runs the project's own build
//! logic. These tests pass [`ProjectExecutionTrust::Trusted`] because
//! the fixture is a committed, reviewed, four-project solution with no
//! custom targets -- an explicit decision about a known tree, which is
//! the only kind of trust decision this design admits. The last test
//! here runs the same Workspace untrusted and asserts that nothing is
//! loaded and nothing is claimed.

use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
    process,
    sync::Arc,
    time::Duration,
};

use brainprint_core::WorkspaceId;
use brainprint_engine::{
    config::WorkspaceConfig,
    csharp_semantic::{
        CSharpHost, CSharpInstall, CSharpLauncher, ProjectExecutionTrust, RefreshRequest,
        adapter::CSharpQueries,
        capability_report, lifecycle,
        protocol::{CSharpRequest, CSharpResponse, path_to_uri},
        refresh_resource, toolchain_identity,
    },
    graph::{GraphEndpoint, RelationKind},
    impact::{Budget, ImpactIntent, ImpactTraversal},
    logical_symbol,
    prepare::InspectPreparer,
    related_tests::RelatedTests,
    relations::{Direction, RelationIndex},
    resolution::Dispatch,
    resolution::Support,
    resource::{Resource, ResourceLanguage, ResourceStore},
    runtime::{
        CancelToken, RequestFailure, RuntimePolicy, SemanticBackendLauncher, SemanticRuntimeHost,
        SemanticRuntimeSupervisor,
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

/// How long a project load may take before the test gives up.
///
/// Generous on purpose: a cold restore on a loaded machine is slow, and
/// a flaky timeout would turn a real signal into noise. The barrier is
/// still a signal -- it returns the moment the server announces.
/// The test project's source, which declares a field typed by the
/// fixture's partial type.
const TEST_FILE: &str = "tests/Core.Tests/RunnerTests.cs";

const LOAD_TIMEOUT: Duration = Duration::from_secs(180);

// ---------------------------------------------------------------------
// Fixture plumbing
// ---------------------------------------------------------------------

fn install_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/csharp_semantic_spike")
}

fn fixture_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/workspaces/csharp-semantic-spike")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("destination");
    for entry in fs::read_dir(from).expect("read fixture") {
        let entry = entry.expect("entry");
        let name = entry.file_name();
        // A previous build's output holds absolute paths from wherever
        // it was produced; copying it is how a probe hangs.
        if name == "bin" || name == "obj" {
            continue;
        }
        let target = to.join(&name);
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy");
        }
    }
}

/// Asks the live host directly.
struct HostQueries<'a> {
    host: &'a CSharpHost,
    cancel: CancelToken,
}

impl CSharpQueries for HostQueries<'_> {
    fn call(&self, request: &CSharpRequest) -> Result<CSharpResponse, RequestFailure> {
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

impl lifecycle::ProjectLoadBarrier for HostQueries<'_> {
    fn completions_seen(&self) -> u64 {
        self.host.load_completions()
    }

    fn wait_for_project_load(&self, seen: u64) -> Result<usize, String> {
        if self.host.wait_for_project_load(seen, LOAD_TIMEOUT) {
            Ok(1)
        } else {
            Err(format!(
                "the server did not announce project initialization within {LOAD_TIMEOUT:?}"
            ))
        }
    }
}

/// One indexed Workspace with one Roslyn language server behind it.
struct Slice {
    workspace: PathBuf,
    db_path: PathBuf,
    base: PathBuf,
    context: AnalysisContext,
    launcher: Arc<CSharpLauncher>,
    trust: ProjectExecutionTrust,
}

impl Slice {
    fn open(label: &str, uid: u8, install: &CSharpInstall, trust: ProjectExecutionTrust) -> Self {
        let base = env::temp_dir().join(format!("brainprint-i4csharp-{label}-{}", process::id()));
        let _ = fs::remove_dir_all(&base);
        let workspace = base.join("workspace");
        copy_tree(&fixture_source(), &workspace);
        if trust.may_load_projects() {
            restore(&workspace);
        }

        let db_path = base.join("data").join("index.db");
        BaselineScan::open(&db_path)
            .expect("index.db")
            .run_initial_scan(&workspace, &WorkspaceConfig::default(), "workspace-rev-1")
            .expect("baseline scan");

        let environment = {
            let index = SemanticIndex::open(&db_path).expect("index.db");
            let projects =
                lifecycle::discover_projects_under(index.connection(), trust, Some(&workspace))
                    .expect("projects");
            lifecycle::environment_identity(install, &projects).expect("environment")
        };
        let context = AnalysisContext {
            workspace: WorkspaceId::from_bytes([uid; 16]),
            backend: SemanticBackendKind::CSharp,
            language: ResourceLanguage::CSharp,
            project_root: ProjectRootIdentity::Key(format!("csharp-spike-{label}")),
            toolchain: toolchain_identity(install, &environment),
        };
        Self {
            workspace,
            db_path,
            base: base.clone(),
            context,
            launcher: Arc::new(CSharpLauncher::new(
                install.clone(),
                trust,
                base.join("server-logs"),
            )),
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

    /// Withdraw every semantic contribution this context published.
    ///
    /// The documented order: withdraw, then replace structurally, then
    /// refresh. `semantic_evidence.relation_id` has no cascade, so a
    /// structural replacement that runs first leaves evidence pointing
    /// at relations whose Symbols are gone -- and this harness replays a
    /// whole baseline scan, so every owner is replaced at once.
    fn withdraw_all(&self) {
        let index = SemanticIndex::open(&self.db_path).expect("index.db");
        let owners: BTreeSet<_> = index
            .owners_of_context(&self.context.context_key())
            .expect("owners")
            .into_iter()
            .collect();
        lifecycle::withdraw_affected(&index, &owners, "TEST_STRUCTURAL_REPLACEMENT")
            .expect("withdraw");
    }

    fn rescan(&self, revision: &str) {
        BaselineScan::open(&self.db_path)
            .expect("index.db")
            .run_initial_scan(&self.workspace, &WorkspaceConfig::default(), revision)
            .expect("baseline scan");
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
            .filter(|resource| resource.language == Some(ResourceLanguage::CSharp))
            .map(|resource| resource.path_key)
            .collect();
        found.sort();
        found
    }

    fn symbol(&self, rel: &str, qualified_name: &str) -> Symbol {
        self.symbols(rel)
            .into_iter()
            .find(|symbol| symbol.qualified_name == qualified_name)
            .unwrap_or_else(|| panic!("{qualified_name} in {rel}"))
    }

    fn symbols(&self, rel: &str) -> Vec<Symbol> {
        SymbolStore::open(&self.db_path)
            .expect("index.db")
            .list_for_resource(self.resource(rel).id)
            .expect("symbols")
    }

    fn write(&self, rel: &str, contents: &str) {
        let path = self.workspace.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(path, contents).expect("write");
    }

    fn text(&self, rel: &str) -> String {
        fs::read_to_string(self.workspace.join(rel)).expect("source")
    }

    /// Start a server, load the solution under the slice's trust, and
    /// run `body` against it.
    fn with_host<T>(&self, body: impl FnOnce(&HostQueries<'_>) -> T) -> T {
        let host = self
            .launcher
            .start(&self.binding())
            .expect("server started");
        let queries = HostQueries {
            host: &host,
            cancel: CancelToken::new(),
        };
        if self.trust.may_load_projects() {
            let seen = host.load_completions();
            queries
                .call(&CSharpRequest::OpenSolution {
                    uri: path_to_uri(&self.workspace.join("CSharpSemanticSpike.sln")),
                })
                .expect("solution opened");
            assert!(
                host.wait_for_project_load(seen, LOAD_TIMEOUT),
                "the server must announce project initialization"
            );
        }
        // Hand the backend Brainprint's current bytes for every source.
        // The measured requirement: a watched-file notification alone is
        // not enough for the server to answer at current positions.
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
        brainprint_engine::runtime::SemanticRuntimeHost::shutdown(&host);
        answer
    }

    /// Refresh one Resource through the whole tier.
    fn refresh(
        &self,
        queries: &HostQueries<'_>,
        rel: &str,
    ) -> brainprint_engine::csharp_semantic::RefreshOutcome {
        let index = SemanticIndex::open(&self.db_path).expect("index.db");
        let projects = lifecycle::discover_projects_under(
            index.connection(),
            self.trust,
            Some(&self.workspace),
        )
        .expect("projects");
        let config = projects.basis();
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
                projects: &projects,
            },
        )
        .unwrap_or_else(|error| panic!("refresh {rel}: {error}"))
    }

    /// Where one occurrence's relation points, read back through the
    /// ordinary graph API.
    fn target_at(&self, rel: &str, start: usize, end: usize) -> Option<GraphEndpoint> {
        let owner = self.resource(rel).id;
        let index = RelationIndex::open(&self.db_path).expect("index.db");
        let symbols = self.symbols(rel);
        let enclosing = symbols
            .iter()
            .filter(|symbol| symbol.span.start_byte <= start && end <= symbol.span.end_byte)
            .min_by_key(|symbol| symbol.span.end_byte - symbol.span.start_byte)?;
        for source in [
            GraphEndpoint::Symbol(enclosing.id),
            GraphEndpoint::Resource(owner),
        ] {
            let found = index
                .outgoing(
                    &source,
                    &[
                        RelationKind::Calls,
                        RelationKind::References,
                        RelationKind::UsesType,
                        RelationKind::Extends,
                        RelationKind::Implements,
                        RelationKind::Overrides,
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
                .map(|relation| relation.target);
            if found.is_some() {
                return found;
            }
        }
        None
    }

    /// Every C# Resource, refreshed. What a whole-Workspace semantic
    /// pass produces, which is what the query APIs then read.
    fn refresh_all(&self, queries: &HostQueries<'_>) {
        for rel in self.sources() {
            self.refresh(queries, &rel);
        }
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

    /// Every confirmed relation into `into`, with its dispatch.
    fn incoming_dispatch(
        &self,
        into: &GraphEndpoint,
        kinds: &[RelationKind],
    ) -> Vec<(GraphEndpoint, Dispatch)> {
        RelationIndex::open(&self.db_path)
            .expect("index.db")
            .incoming(into, kinds)
            .expect("incoming")
            .confirmed
            .into_iter()
            .map(|relation| (relation.source, relation.dispatch))
            .collect()
    }

    /// A readable name for an endpoint, for assertion messages.
    fn name_of(&self, endpoint: &GraphEndpoint) -> String {
        match endpoint {
            GraphEndpoint::Symbol(symbol) => Connection::open(&self.db_path)
                .expect("index.db")
                .query_row(
                    "SELECT resource.path_key || '::' || symbol.qualified_name FROM symbol \
                     JOIN resource ON resource.id = symbol.resource_id WHERE symbol.uid = ?1",
                    rusqlite::params![symbol.to_bytes().to_vec()],
                    |row| row.get::<_, String>(0),
                )
                .unwrap_or_else(|_| format!("{symbol:?}")),
            other => format!("{other:?}"),
        }
    }

    fn names(&self, endpoints: &[GraphEndpoint]) -> Vec<String> {
        let mut found: Vec<String> = endpoints.iter().map(|one| self.name_of(one)).collect();
        found.sort();
        found.dedup();
        found
    }

    /// The single symbol whose qualified name matches, in one file.
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

    /// The symbol whose declaration contains `needle`, for the several
    /// overloads and same-name members the fixture keeps as traps.
    fn symbol_with(&self, rel: &str, qualified_name: &str, needle: &str) -> Symbol {
        let text = self.text(rel);
        self.symbols(rel)
            .into_iter()
            .filter(|symbol| symbol.qualified_name == qualified_name)
            .find(|symbol| {
                text.get(symbol.span.start_byte..symbol.span.end_byte)
                    .is_some_and(|body| body.contains(needle))
            })
            .unwrap_or_else(|| panic!("{qualified_name} containing {needle:?} in {rel}"))
    }

    fn offset_of(&self, rel: &str, needle: &str, nth: usize) -> usize {
        let text = self.text(rel);
        text.match_indices(needle)
            .nth(nth)
            .unwrap_or_else(|| panic!("{needle:?} not in {rel}"))
            .0
    }
}

impl Drop for Slice {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

/// Restore the copied solution, so the server has a package graph to
/// load.
///
/// `bin` and `obj` are never copied -- a previous build's output holds
/// absolute paths from wherever it was produced -- so the copy starts
/// unrestored, and a project with no `project.assets.json` never
/// finishes loading. Restoring is itself project execution, which is
/// exactly why only the trusted slices do it.
///
/// Discovery already prunes `bin` and `obj` beside a `.csproj`, so what
/// this produces never becomes a Resource.
fn restore(workspace: &Path) {
    let output = process::Command::new(dotnet())
        .arg("restore")
        .arg("CSharpSemanticSpike.sln")
        .current_dir(workspace)
        .output()
        .expect("dotnet restore ran");
    assert!(
        output.status.success(),
        "dotnet restore failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The .NET SDK. Supplied, never searched for by the engine itself.
fn dotnet() -> String {
    env::var("BRAINPRINT_DOTNET").unwrap_or_else(|_| "dotnet".to_owned())
}

/// The install, or the reason the suite cannot run.
fn install_or_skip() -> Option<CSharpInstall> {
    match CSharpInstall::locate(&install_root()) {
        Ok(install) => Some(install),
        Err(error) => {
            eprintln!("skipping C# acceptance: {error}");
            eprintln!("run scripts/csharp_semantic_spike/restore.sh first");
            None
        }
    }
}

// ---------------------------------------------------------------------
// Overload resolution
// ---------------------------------------------------------------------

/// Three same-named methods, three different declarations.
///
/// The capability a structural tier cannot fake: `Parse("x")` and
/// `Parse(1)` differ only in an argument type, and nothing short of a
/// compiler knows which declaration each one names.
#[test]
#[ignore = "needs the restored Roslyn language server"]
fn exact_overloads_resolve_to_their_own_declarations() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("overloads", 21, &install, ProjectExecutionTrust::Trusted);
    slice.with_host(|queries| {
        let outcome = slice.refresh(queries, "src/App/Program.cs");
        assert!(outcome.evidence_count > 0, "{:?}", outcome.report);

        let string_overload = slice.symbol("src/Core/Overloads.cs", "Core.Overloads.Parse");
        let all_parse: Vec<Symbol> = slice
            .symbols("src/Core/Overloads.cs")
            .into_iter()
            .filter(|symbol| symbol.qualified_name == "Core.Overloads.Parse")
            .collect();
        assert!(
            all_parse.len() >= 2,
            "the fixture declares several Parse overloads"
        );

        let first = slice.offset_of("src/App/Program.cs", "Overloads.Parse(\"x\")", 0);
        let second = slice.offset_of("src/App/Program.cs", "Overloads.Parse(1)", 0);
        let first_target =
            slice.target_at("src/App/Program.cs", first, first + "Overloads.Parse".len());
        let second_target = slice.target_at(
            "src/App/Program.cs",
            second,
            second + "Overloads.Parse".len(),
        );

        assert!(first_target.is_some(), "Parse(\"x\") resolved");
        assert!(second_target.is_some(), "Parse(1) resolved");
        assert_ne!(
            first_target, second_target,
            "two overloads are two declarations, not one name"
        );
        let _ = string_overload;
    });
}

// ---------------------------------------------------------------------
// Partial identity
// ---------------------------------------------------------------------

/// The fixture's partial type is one logical symbol owning two
/// declarations, and the reference binds to it exactly once.
///
/// This is the shape the whole task turned on: the server answers a
/// reference to `Runner` with *both* `Runner.Part1.cs` and
/// `Runner.Part2.cs`, and the graph has to say "one type" without ever
/// saying "one occurrence, two relations".
#[test]
#[ignore = "needs the restored Roslyn language server"]
fn a_partial_type_is_one_logical_symbol_with_two_declarations() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("partial", 22, &install, ProjectExecutionTrust::Trusted);
    slice.with_host(|queries| {
        let outcome = slice.refresh(queries, TEST_FILE);
        assert_eq!(
            outcome.grouped.len(),
            1,
            "one partial type, one group: {:?}",
            outcome.report
        );

        let logical = *outcome.grouped.iter().next().expect("group");
        let index = SemanticIndex::open(&slice.db_path).expect("index.db");
        let declarations =
            logical_symbol::declarations(index.connection(), logical).expect("declarations");
        assert_eq!(declarations.len(), 2, "both declarations belong to it");

        let files: BTreeSet<String> = declarations
            .iter()
            .filter_map(|symbol| {
                index
                    .connection()
                    .query_row(
                        "SELECT resource.path_key FROM symbol \
                         JOIN resource ON resource.id = symbol.resource_id \
                         WHERE symbol.uid = ?1",
                        rusqlite::params![symbol.to_bytes().to_vec()],
                        |row| row.get::<_, String>(0),
                    )
                    .ok()
            })
            .collect();
        assert_eq!(
            files,
            ["src/Core/Runner.Part1.cs", "src/Core/Runner.Part2.cs"]
                .into_iter()
                .map(str::to_owned)
                .collect::<BTreeSet<_>>()
        );

        // One binding, not two, and not a candidate set.
        let site = slice.offset_of(TEST_FILE, "Runner _shared", 0);
        assert_eq!(
            slice.target_at(TEST_FILE, site, site + "Runner".len()),
            Some(GraphEndpoint::Logical(logical)),
            "the reference binds once, to the group"
        );
        assert!(
            !outcome
                .report
                .iter()
                .any(|line| line.contains("CANDIDATES")),
            "a partial type is not an ambiguity: {:?}",
            outcome.report
        );
    });
}

/// A member declared in the second file is still a member of the type,
/// and a call to it binds to that declaration rather than to the group.
#[test]
#[ignore = "needs the restored Roslyn language server"]
fn a_member_of_a_partial_type_binds_to_its_own_declaration() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open(
        "partial-member",
        23,
        &install,
        ProjectExecutionTrust::Trusted,
    );
    slice.with_host(|queries| {
        slice.refresh(queries, TEST_FILE);
        let site = slice.offset_of(TEST_FILE, "_shared.Compute", 0);
        let target = slice.target_at(TEST_FILE, site, site + "_shared.Compute".len());
        let compute = slice.symbol("src/Core/Runner.Part2.cs", "Core.Runner.Compute");
        assert_eq!(
            target,
            Some(GraphEndpoint::Symbol(compute.id)),
            "a member is one declaration, so it stays a Symbol"
        );
    });
}

// ---------------------------------------------------------------------
// Cross-project and external
// ---------------------------------------------------------------------

/// A reference across a project reference resolves, and a framework call
/// becomes an assembly identity rather than an indexed file.
#[test]
#[ignore = "needs the restored Roslyn language server"]
fn framework_targets_become_assembly_identities() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("external", 24, &install, ProjectExecutionTrust::Trusted);
    slice.with_host(|queries| {
        let outcome = slice.refresh(queries, "src/App/Program.cs");
        let site = slice.offset_of("src/App/Program.cs", "Console.WriteLine", 0);
        let target = slice.target_at("src/App/Program.cs", site, site + "Console.WriteLine".len());

        match target {
            Some(GraphEndpoint::External(entity)) => {
                assert!(
                    entity.package_identity.starts_with("System."),
                    "the assembly identity names the assembly: {entity:?}"
                );
                assert_eq!(
                    entity.declaration_locator, None,
                    "a machine path is never identity"
                );
                let rendered = format!("{entity:?}");
                for leaked in ["/usr/", "/Users/", "/var/folders", ".dll"] {
                    assert!(!rendered.contains(leaked), "{leaked} leaked: {rendered}");
                }
            }
            other => panic!(
                "expected an external assembly, got {other:?}: {:?}",
                outcome.report
            ),
        }

        // And nothing from the framework was indexed as a Resource.
        assert!(
            slice
                .resources()
                .iter()
                .all(|resource| !resource.path_key.contains("dotnet")),
            "no reference pack becomes a Resource"
        );
    });
}

// ---------------------------------------------------------------------
// Freshness
// ---------------------------------------------------------------------

/// An edit to an existing document becomes current, with no reload.
///
/// The document synchronization barrier is the whole content path, and
/// what makes it work is the two rules the live server enforces: one
/// `didOpen` per document, and a change that carries the range it
/// replaces. The second is asserted here by proxy -- a rangeless change
/// kills the connection, so a green run means the range was right.
///
/// The staleness this guards against is a plausible, wrong location --
/// an old span in a file that no longer declares the member -- not an
/// empty answer. Nothing downstream could catch it by shape, so it has
/// to be caught here.
#[test]
#[ignore = "needs the restored Roslyn language server"]
fn a_content_edit_is_current_after_the_document_barrier() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("edit", 25, &install, ProjectExecutionTrust::Trusted);
    slice.with_host(|queries| {
        slice.refresh(queries, TEST_FILE);
        let before = slice.symbol("src/Core/Runner.Part2.cs", "Core.Runner.Compute");

        // Move `Compute` from Part2 to Part1: the same member of the
        // same type, declared in the other file.
        slice.write(
            "src/Core/Runner.Part2.cs",
            "using Contracts;\n\nnamespace Core;\n\npublic partial class Runner\n{\n\
             \x20   private int Extra() => 1;\n\n\
             \x20   void IRunner.Run() { }\n}\n",
        );
        slice.write(
            "src/Core/Runner.Part1.cs",
            "using Contracts;\n\nnamespace Core;\n\n\
             public partial class Runner : BaseRunner, IRunner, INamed\n{\n\
             \x20   public override void Run() { }\n\n\
             \x20   public string Name => \"runner\";\n\n\
             \x20   public override int Compute(int seed) => seed + Extra();\n}\n",
        );
        slice.withdraw_all();
        slice.rescan("workspace-rev-2");

        let changes: Vec<lifecycle::ResourceChange> =
            ["src/Core/Runner.Part1.cs", "src/Core/Runner.Part2.cs"]
                .into_iter()
                .map(|rel| {
                    lifecycle::ResourceChange::new(
                        slice.resource(rel).id,
                        lifecycle::ChangeKind::Changed,
                        rel,
                    )
                })
                .collect();
        for change in &changes {
            assert_eq!(
                change.class(),
                lifecycle::ChangeClass::DocumentContent,
                "editing a body is a content change"
            );
        }
        lifecycle::synchronize_documents(queries, queries, &slice.workspace, &changes)
            .expect("document sync");

        // The document itself is current -- this is what makes the
        // staleness below so easy to miss.
        let structure = queries
            .call(&CSharpRequest::DocumentSymbol {
                uri: path_to_uri(&slice.workspace.join("src/Core/Runner.Part2.cs")),
            })
            .expect("document symbols");
        let CSharpResponse::DocumentSymbols(members) = structure else {
            panic!("document symbols");
        };
        assert!(
            !members.iter().any(|member| member.name.contains("Compute")),
            "the server's copy of Part2 is current: {members:?}"
        );

        slice.refresh(queries, TEST_FILE);
        let site = slice.offset_of(TEST_FILE, "_shared.Compute", 0);
        let target = slice.target_at(TEST_FILE, site, site + "_shared.Compute".len());
        let moved = slice.symbol("src/Core/Runner.Part1.cs", "Core.Runner.Compute");

        assert_ne!(
            before.id, moved.id,
            "the declaration really did move to another file"
        );
        assert_eq!(
            target,
            Some(GraphEndpoint::Symbol(moved.id)),
            "the answer follows the declaration to its new file"
        );
    });
}

/// A new declaration is current after the project reload, and joins the
/// group that already existed.
///
/// Adding a `.cs` changes a project's Compile item set without touching
/// any project file, which is why the reload has to name the project
/// that *owns* it -- naming only changed project files reloads nothing
/// and then waits for an announcement that never comes.
///
/// The second assertion is the identity rule: a third declaration joins
/// the existing group rather than making a new one, because identity
/// comes from semantic meaning and never from the set of files that
/// happen to declare it.
#[test]
#[ignore = "needs the restored Roslyn language server"]
fn a_new_declaration_is_current_after_a_project_reload() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("reload", 26, &install, ProjectExecutionTrust::Trusted);
    slice.write(
        "src/Core/Runner.Part3.cs",
        "namespace Core;\n\npublic partial class Runner\n{\n\
         \x20   public int Third() => 3;\n}\n",
    );
    slice.rescan("workspace-rev-2");

    let change = lifecycle::ResourceChange::new(
        slice.resource("src/Core/Runner.Part3.cs").id,
        lifecycle::ChangeKind::Added,
        "src/Core/Runner.Part3.cs",
    );
    assert_eq!(
        change.class(),
        lifecycle::ChangeClass::ProjectStructure,
        "a new .cs joins the project under the default glob"
    );

    slice.with_host(|queries| {
        let projects = {
            let index = SemanticIndex::open(&slice.db_path).expect("index.db");
            lifecycle::discover_projects_under(
                index.connection(),
                ProjectExecutionTrust::Trusted,
                Some(&slice.workspace),
            )
            .expect("projects")
        };
        assert_eq!(
            lifecycle::projects_to_reload(std::slice::from_ref(&change), &projects)
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["src/Core/Core.csproj".to_owned()],
            "a new .cs reloads the project that owns it, not nothing"
        );
        lifecycle::reload_projects(
            queries,
            queries,
            &slice.workspace,
            std::slice::from_ref(&change),
            &projects,
        )
        .expect("the barrier fires");

        let outcome = slice.refresh(queries, TEST_FILE);
        let logical = *outcome.grouped.iter().next().expect("group");
        let index = SemanticIndex::open(&slice.db_path).expect("index.db");
        assert_eq!(
            logical_symbol::declarations(index.connection(), logical)
                .expect("declarations")
                .len(),
            3,
            "the third declaration joined the existing group"
        );
    });
}

// ---------------------------------------------------------------------
// Trust
// ---------------------------------------------------------------------

/// The same Workspace, untrusted: no project is loaded, no project code
/// runs, and nothing cross-project is claimed.
///
/// The point of this test is what it does *not* find. An untrusted pass
/// that quietly produced the trusted answers would mean the trust gate
/// was decorative.
#[test]
#[ignore = "needs the restored Roslyn language server"]
fn an_untrusted_workspace_loads_nothing_and_claims_nothing() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("untrusted", 28, &install, ProjectExecutionTrust::Untrusted);
    slice.with_host(|queries| {
        // The gate is in the host, so even a direct call is refused.
        let refused = queries.call(&CSharpRequest::OpenSolution {
            uri: path_to_uri(&slice.workspace.join("CSharpSemanticSpike.sln")),
        });
        assert!(refused.is_err(), "an untrusted host must refuse to load");

        let outcome = slice.refresh(queries, TEST_FILE);
        assert!(
            outcome.grouped.is_empty(),
            "no compilation, so no partial identity: {:?}",
            outcome.report
        );

        let capabilities = capability_report(&slice.context, ProjectExecutionTrust::Untrusted);
        assert_eq!(
            capabilities.support(SemanticCapability::CallsCrossFile),
            Support::Unsupported
        );
        assert_eq!(
            capabilities.support(SemanticCapability::SyntaxStructure),
            Support::Supported
        );
    });
}

/// Granting trust invalidates what was published without it.
#[test]
#[ignore = "needs the restored Roslyn language server"]
fn granting_trust_invalidates_an_untrusted_publication() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("regrant", 29, &install, ProjectExecutionTrust::Untrusted);
    let index = SemanticIndex::open(&slice.db_path).expect("index.db");
    let untrusted =
        lifecycle::discover_projects(index.connection(), ProjectExecutionTrust::Untrusted)
            .expect("untrusted")
            .basis();
    let trusted = lifecycle::discover_projects(index.connection(), ProjectExecutionTrust::Trusted)
        .expect("trusted")
        .basis();
    assert_ne!(
        untrusted.fingerprint(),
        trusted.fingerprint(),
        "trust is part of what a publication was based on"
    );
}

// ---------------------------------------------------------------------
// The whole-Workspace slice
// ---------------------------------------------------------------------

/// One pass over the whole fixture, read back through the ordinary
/// query APIs.
///
/// Everything asserted here travels `Roslyn → adapter → evidence →
/// publication → merge → query`. A probe that got the right answer out
/// of the backend proves nothing on its own; what matters is that the
/// canonical graph says it afterwards, to a caller who has never heard
/// of C#.
///
/// One test rather than twelve because one solution load is eight
/// seconds and the assertions are independent of each other.
#[test]
#[ignore = "needs the restored Roslyn language server"]
fn the_csharp_slice_holds_against_the_real_backend() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("slice", 30, &install, ProjectExecutionTrust::Trusted);
    slice.with_host(|queries| {
        slice.refresh_all(queries);

        // ---- Alias and global using -------------------------------
        //
        // `using Alias = Contracts.Model;` and a same-named
        // `Contracts.Other.Model` that must not win.
        let alias_site = slice.offset_of("src/App/Program.cs", "Alias Aliased", 0);
        let alias_target =
            slice.target_at("src/App/Program.cs", alias_site, alias_site + "Alias".len());
        let model = slice.only("src/Contracts/Models.cs", "Contracts.Model");
        let trap = slice.only("src/Contracts/Traps.cs", "Contracts.Other.Model");
        assert_eq!(
            alias_target,
            Some(GraphEndpoint::Symbol(model.id)),
            "an alias resolves to the type it was bound to, not to the name"
        );
        assert_ne!(alias_target, Some(GraphEndpoint::Symbol(trap.id)));

        // `RunnerTests.cs` has no file-level `using Core;` -- only the
        // project-wide `global using Core;` -- so this binding exists
        // only if the compiler's view of imports was used.
        let global_site = slice.offset_of(TEST_FILE, "Runner _shared", 0);
        assert!(
            matches!(
                slice.target_at(TEST_FILE, global_site, global_site + "Runner".len()),
                Some(GraphEndpoint::Logical(_))
            ),
            "a type reached through `global using` still binds"
        );

        // ---- Overloads --------------------------------------------
        let string_overload = slice.symbol_with(
            "src/Core/Overloads.cs",
            "Core.Overloads.Parse",
            "string value",
        );
        let int_overload =
            slice.symbol_with("src/Core/Overloads.cs", "Core.Overloads.Parse", "int value");
        let object_overload = slice.symbol_with(
            "src/Core/Overloads.cs",
            "Core.Overloads.Parse",
            "object value",
        );
        for (call, expected, label) in [
            ("Overloads.Parse(\"x\")", string_overload.id, "string"),
            ("Overloads.Parse(1)", int_overload.id, "int"),
        ] {
            let site = slice.offset_of("src/App/Program.cs", call, 0);
            assert_eq!(
                slice.target_at("src/App/Program.cs", site, site + "Overloads.Parse".len()),
                Some(GraphEndpoint::Symbol(expected)),
                "{call} selects the {label} overload"
            );
        }
        assert_ne!(string_overload.id, int_overload.id);
        assert_ne!(string_overload.id, object_overload.id);

        // A generic method is its own declaration, not one of the
        // `Parse` family.
        let convert = slice.only("src/Core/Overloads.cs", "Core.Overloads.Convert");
        let convert_site = slice.offset_of("src/App/Program.cs", "Overloads.Convert", 0);
        assert_eq!(
            slice.target_at(
                "src/App/Program.cs",
                convert_site,
                convert_site + "Overloads.Convert".len()
            ),
            Some(GraphEndpoint::Symbol(convert.id)),
            "a generic method resolves to its own declaration"
        );

        // ---- References -------------------------------------------
        //
        // Read as incoming edges on the declaration, which is what
        // "who uses this" is in the canonical graph.
        let parse_users = slice.incoming(
            &GraphEndpoint::Symbol(string_overload.id),
            &[RelationKind::Calls, RelationKind::References],
        );
        assert!(
            !parse_users.is_empty(),
            "the string overload has callers: {:?}",
            slice.names(&parse_users)
        );
        let users = slice.names(&parse_users);
        assert!(
            users.iter().any(|name| name.starts_with("src/App/")),
            "including one in another project: {users:?}"
        );

        // A reference to a partial type is one binding to the group,
        // however many declarations Roslyn reported.
        let logical = match slice.target_at(TEST_FILE, global_site, global_site + "Runner".len()) {
            Some(GraphEndpoint::Logical(logical)) => logical,
            other => panic!("expected the group, got {other:?}"),
        };
        let group_users = slice.incoming(
            &GraphEndpoint::Logical(logical),
            &[RelationKind::References, RelationKind::UsesType],
        );
        assert!(
            !group_users.is_empty(),
            "the group is what references point at"
        );

        // ---- Calls and honest dispatch -----------------------------
        let base_run = slice.only("src/Core/BaseRunner.cs", "Core.BaseRunner.Run");
        let not_virtual = slice.only("src/Core/BaseRunner.cs", "Core.BaseRunner.NotVirtual");
        let virtual_callers =
            slice.incoming_dispatch(&GraphEndpoint::Symbol(base_run.id), &[RelationKind::Calls]);
        assert!(
            !virtual_callers.is_empty(),
            "`based.Run()` binds to the declaration its static type names"
        );
        assert!(
            virtual_callers
                .iter()
                .all(|(_, dispatch)| *dispatch == Dispatch::Unknown),
            "a virtual callee is not what necessarily runs: {virtual_callers:?}"
        );
        let final_callers = slice.incoming_dispatch(
            &GraphEndpoint::Symbol(not_virtual.id),
            &[RelationKind::Calls],
        );
        assert!(
            !final_callers.is_empty()
                && final_callers
                    .iter()
                    .all(|(_, dispatch)| *dispatch == Dispatch::Static),
            "a non-virtual callee is bound exactly where it points: {final_callers:?}"
        );

        // ---- Extension method --------------------------------------
        let label = slice.only("src/Core/Extensions.cs", "Core.RunnerExtensions.Label");
        let unrelated_label =
            slice.only("src/Core/Extensions.cs", "Core.UnrelatedExtensions.Label");
        let label_site = slice.offset_of("src/App/Program.cs", "runner.Label()", 0);
        assert_eq!(
            slice.target_at(
                "src/App/Program.cs",
                label_site,
                label_site + "runner.Label".len()
            ),
            Some(GraphEndpoint::Symbol(label.id)),
            "an extension call targets the static declaration that defines it"
        );
        assert!(
            slice
                .incoming(
                    &GraphEndpoint::Symbol(unrelated_label.id),
                    &[RelationKind::Calls]
                )
                .is_empty(),
            "and not the one that merely shares its name"
        );

        // ---- Overrides ---------------------------------------------
        let middle_run = slice.only("src/Core/Sealed.cs", "Core.Middle.Run");
        let leaf_run = slice.only("src/Core/Sealed.cs", "Core.Leaf.Run");
        let middle = slice.only("src/Core/Sealed.cs", "Core.Middle");
        assert_eq!(
            slice.outgoing(
                &GraphEndpoint::Symbol(middle_run.id),
                &[RelationKind::Overrides]
            ),
            vec![GraphEndpoint::Symbol(base_run.id)],
            "virtual -> override"
        );
        assert_eq!(
            slice.outgoing(
                &GraphEndpoint::Symbol(leaf_run.id),
                &[RelationKind::Overrides]
            ),
            vec![GraphEndpoint::Symbol(middle_run.id)],
            "a sealed override overrides the nearest declaration, transitively"
        );
        let _ = middle;

        // The partial case: `Compute` is declared in `Part2`, and the
        // base list is written in `Part1`. A derivation that read only
        // its own declaration's edges would find no ancestor at all.
        let compute = slice.only("src/Core/Runner.Part2.cs", "Core.Runner.Compute");
        let base_compute = slice.only("src/Core/BaseRunner.cs", "Core.BaseRunner.Compute");
        assert_eq!(
            slice.outgoing(
                &GraphEndpoint::Symbol(compute.id),
                &[RelationKind::Overrides]
            ),
            vec![GraphEndpoint::Symbol(base_compute.id)],
            "abstract -> override, across two declarations of one type"
        );

        // The traps declare `Run` and override nothing.
        for (file, name) in [
            ("src/Core/BaseRunner.cs", "Core.Unrelated.Run"),
            ("src/Core/Implementers.cs", "Core.NotARunner.Run"),
        ] {
            let trap = slice.only(file, name);
            assert!(
                slice
                    .outgoing(&GraphEndpoint::Symbol(trap.id), &[RelationKind::Overrides])
                    .is_empty(),
                "{name} shares a name and nothing else"
            );
        }

        // ---- Implementation target ---------------------------------
        let interface_run = slice.only("src/Contracts/IRunner.cs", "Contracts.IRunner.Run");
        let implementers = slice.incoming(
            &GraphEndpoint::Symbol(interface_run.id),
            &[RelationKind::Implements],
        );
        let found = slice.names(&implementers);
        assert!(
            found
                .iter()
                .any(|name| name == "src/Core/Implementers.cs::Core.OtherRunner.Run"),
            "the implementation query finds every implementing member: {found:?}"
        );
        assert!(
            found
                .iter()
                .any(|name| name == "src/Core/Runner.Part2.cs::Core.Runner.Run"),
            "including the explicit `void IRunner.Run()`: {found:?}"
        );
        for excluded in [
            "src/Core/BaseRunner.cs::Core.Unrelated.Run",
            "src/Core/Implementers.cs::Core.NotARunner.Run",
            // The implicit `Run` in Part1 overrides the base; the
            // explicit declaration is what the interface reaches.
            "src/Core/Runner.Part1.cs::Core.Runner.Run",
        ] {
            assert!(
                !found.iter().any(|name| name == excluded),
                "{excluded} does not implement IRunner.Run: {found:?}"
            );
        }

        // ---- Type surface ------------------------------------------
        for (rel, name) in [
            ("src/Contracts/Models.cs", "Contracts.Model"),
            ("src/Contracts/Models.cs", "Contracts.Tally"),
            ("src/Contracts/Models.cs", "Contracts.Level"),
            ("src/Contracts/Models.cs", "Contracts.Snapshot"),
            ("src/Contracts/Models.cs", "Contracts.Box"),
            ("src/Contracts/IRunner.cs", "Contracts.IRunner"),
        ] {
            let declared = slice.only(rel, name);
            assert!(
                declared.span.end_byte > declared.span.start_byte,
                "{name} is a declaration with a span"
            );
        }
        // A generic type used as a return type resolves to the generic
        // declaration, not to a constructed instantiation.
        // A parameter type and a field type are anchored by I3, and
        // both resolve. A *return* type and a generic type argument are
        // not anchored -- `Box<Model> Wrap(Model model)` gives one type
        // site, on the parameter -- so there is nothing for this tier
        // to prove about them and the capability says PARTIAL rather
        // than this test pretending otherwise.
        let model_parameter = slice.offset_of("src/Core/Overloads.cs", "Model model", 0);
        assert_eq!(
            slice.target_at(
                "src/Core/Overloads.cs",
                model_parameter,
                model_parameter + "Model".len()
            ),
            Some(GraphEndpoint::Symbol(model.id)),
            "a parameter type resolves"
        );
        let boxed = slice.only("src/Contracts/Models.cs", "Contracts.Box");
        assert!(
            slice
                .incoming(&GraphEndpoint::Symbol(boxed.id), &[RelationKind::UsesType])
                .is_empty(),
            "and a return type is not anchored, so it is a gap rather than a guess"
        );

        // ---- Cross-project ------------------------------------------
        //
        // App -> Core -> Contracts, and Core.Tests -> Core, all proved
        // rather than assumed from the project references.
        let runner_part1 = slice.only("src/Core/Runner.Part1.cs", "Core.Runner");
        let irunner = slice.only("src/Contracts/IRunner.cs", "Contracts.IRunner");
        assert!(
            slice
                .outgoing(
                    &GraphEndpoint::Symbol(runner_part1.id),
                    &[RelationKind::Implements]
                )
                .contains(&GraphEndpoint::Symbol(irunner.id)),
            "Core -> Contracts, at the type level"
        );
    });
}

// ---------------------------------------------------------------------
// Agent-facing surfaces
// ---------------------------------------------------------------------

/// Impact, related tests and prepared source, through the unchanged
/// common APIs.
#[test]
#[ignore = "needs the restored Roslyn language server"]
fn the_agent_surfaces_answer_for_csharp() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("surfaces", 31, &install, ProjectExecutionTrust::Trusted);
    slice.with_host(|queries| {
        slice.refresh_all(queries);

        let base_run = slice.only("src/Core/BaseRunner.cs", "Core.BaseRunner.Run");
        let middle_run = slice.only("src/Core/Sealed.cs", "Core.Middle.Run");
        let impact = ImpactTraversal::open(&slice.db_path)
            .expect("index.db")
            .run(
                ImpactIntent::PublicSignatureChange,
                &GraphEndpoint::Symbol(base_run.id),
                &Budget::default(),
            )
            .expect("impact");
        assert!(
            impact
                .nodes
                .iter()
                .any(|node| node.endpoint == GraphEndpoint::Symbol(middle_run.id)),
            "a signature change on a virtual member reaches its overrides"
        );

        let irunner = slice.only("src/Contracts/IRunner.cs", "Contracts.IRunner");
        let runner_part1 = slice.only("src/Core/Runner.Part1.cs", "Core.Runner");
        let interface_impact = ImpactTraversal::open(&slice.db_path)
            .expect("index.db")
            .run(
                ImpactIntent::BaseInterfaceChange,
                &GraphEndpoint::Symbol(irunner.id),
                &Budget::default(),
            )
            .expect("impact");
        assert!(
            interface_impact
                .nodes
                .iter()
                .any(|node| node.endpoint == GraphEndpoint::Symbol(runner_part1.id)),
            "an interface change reaches the types that implement it"
        );

        // A partial type's change reaches its consumers without the
        // graph growing one relation per declaration.
        let compute = slice.only("src/Core/Runner.Part2.cs", "Core.Runner.Compute");
        let related = RelatedTests::open(&slice.db_path)
            .expect("index.db")
            .for_target(
                &GraphEndpoint::Symbol(compute.id),
                ImpactIntent::PublicSignatureChange,
                &Budget::default(),
            )
            .expect("related tests");
        assert!(
            related
                .candidates
                .iter()
                .any(|candidate| candidate.path_rel.contains("RunnerTests")),
            "the test that exercises it is reachable: {:?}",
            related
                .candidates
                .iter()
                .map(|candidate| &candidate.path_rel)
                .collect::<Vec<_>>()
        );

        // Prepared inspection: current source, not a line number to go
        // and read. A hard gate.
        let prepared = InspectPreparer::open(&slice.db_path, &slice.workspace)
            .expect("preparer")
            .prepare(
                &GraphEndpoint::Symbol(base_run.id),
                Direction::Incoming,
                &[RelationKind::Calls, RelationKind::Overrides],
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

        // And for a partial type, preparation carries the declaration
        // set -- both files -- rather than a synthetic span for the
        // group, which has no source of its own.
        let logical_site = slice.offset_of(TEST_FILE, "Runner _shared", 0);
        let Some(GraphEndpoint::Logical(logical)) =
            slice.target_at(TEST_FILE, logical_site, logical_site + "Runner".len())
        else {
            panic!("the group");
        };
        let group_prepared = InspectPreparer::open(&slice.db_path, &slice.workspace)
            .expect("preparer")
            .prepare(
                &GraphEndpoint::Logical(logical),
                Direction::Incoming,
                &[RelationKind::References, RelationKind::UsesType],
            )
            .expect("prepared");
        assert!(
            group_prepared.declarations.len() >= 2,
            "a logical type prepares every declaration it owns: {:?}",
            group_prepared.declarations.len()
        );
        assert!(
            group_prepared
                .ranges
                .iter()
                .any(|range| range.source.contains("partial class Runner")),
            "and their current source comes back"
        );
    });
}

// ---------------------------------------------------------------------
// Multi-target
// ---------------------------------------------------------------------

/// One effective target framework, and the other world is a gap.
///
/// The measurement this encodes: the server loads a project that
/// declares two frameworks, announces one initialization for it like
/// any other, and answers from one of them. `Modern.Name` inside
/// `#if NET10_0_OR_GREATER` resolves; `Legacy.Name` in the `#else`
/// branch answers with nothing.
///
/// So the invariant that matters is not "detect the collapse" -- there
/// is nothing to collapse, because the backend never offers two
/// answers. It is that the unanswered world stays unanswered: a
/// declaration reachable only from the other framework's branch must
/// not acquire an edge from somewhere else, and the caller must be able
/// to find out that this is why.
#[test]
#[ignore = "needs the restored Roslyn language server"]
fn a_multi_target_project_claims_one_world_and_gaps_the_other() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("multi", 32, &install, ProjectExecutionTrust::Trusted);
    slice.with_host(|queries| {
        let outcome = slice.refresh(queries, "src/Multi/Surface.cs");

        let index = SemanticIndex::open(&slice.db_path).expect("index.db");
        let config = lifecycle::discover_projects_under(
            index.connection(),
            ProjectExecutionTrust::Trusted,
            Some(&slice.workspace),
        )
        .expect("projects");
        assert!(
            config.is_multi_target(slice.resource("src/Multi/Surface.cs").id),
            "the project's two worlds are observable before anything is claimed"
        );

        let modern = slice.only("src/Multi/Surface.cs", "Multi.Modern");
        let legacy = slice.only("src/Multi/Surface.cs", "Multi.Legacy");
        let modern_users = slice.incoming(
            &GraphEndpoint::Symbol(modern.id),
            &[RelationKind::References, RelationKind::UsesType],
        );
        let legacy_users = slice.incoming(
            &GraphEndpoint::Symbol(legacy.id),
            &[RelationKind::References, RelationKind::UsesType],
        );

        // Exactly one of the two branches produced an edge. Which one
        // is the backend's choice and is not observable here; that both
        // did not is the assertion.
        assert!(
            modern_users.is_empty() != legacy_users.is_empty(),
            "one framework's branch is represented and the other is not: \
             modern={:?} legacy={:?} report={:?}",
            slice.names(&modern_users),
            slice.names(&legacy_users),
            outcome.report
        );

        // And the unrepresented branch is a gap rather than a target
        // borrowed from the world that did answer.
        let unrepresented = if modern_users.is_empty() {
            modern
        } else {
            legacy
        };
        assert!(
            slice
                .incoming(
                    &GraphEndpoint::Symbol(unrepresented.id),
                    &[
                        RelationKind::References,
                        RelationKind::UsesType,
                        RelationKind::Calls,
                    ]
                )
                .is_empty(),
            "the other framework's declaration keeps no invented edge"
        );
    });
}

// ---------------------------------------------------------------------
// The shared runtime
// ---------------------------------------------------------------------

/// Two callers, one server.
///
/// A language server is a process with a compilation in it; one per
/// caller would mean loading the solution again per question.
#[test]
#[ignore = "needs the restored Roslyn language server"]
fn one_analysis_context_runs_one_roslyn_server() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open(
        "shared-runtime",
        33,
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
        "two callers, one compilation"
    );
    drop((first, second));
    supervisor.shutdown();
}

/// A restarted backend holds nothing from the old one.
///
/// Document versions and the set of opened documents belong to a
/// connection: a fresh server has opened nothing, so the first
/// synchronization after a restart must be a `didOpen` again. Reusing
/// the old connection's bookkeeping would send a `didChange` for a
/// document the new server has never seen.
#[test]
#[ignore = "needs the restored Roslyn language server"]
fn a_restarted_backend_reopens_rather_than_changes() {
    let Some(install) = install_or_skip() else {
        return;
    };
    let slice = Slice::open("restart", 34, &install, ProjectExecutionTrust::Trusted);
    let uri = path_to_uri(&slice.workspace.join("src/Core/Runner.Part1.cs"));

    let first = slice
        .launcher
        .start(&slice.binding())
        .expect("first server");
    assert_eq!(
        first.exchange_text(&uri, "class A {}"),
        None,
        "a cold server holds nothing"
    );
    assert_eq!(
        first.exchange_text(&uri, "class B {}").as_deref(),
        Some("class A {}"),
        "and then holds what it was handed"
    );
    let first_version = first.next_document_version();
    SemanticRuntimeHost::shutdown(&first);

    let second = slice.launcher.start(&slice.binding()).expect("restarted");
    assert_eq!(
        second.exchange_text(&uri, "class A {}"),
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
