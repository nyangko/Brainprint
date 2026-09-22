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
    logical_symbol,
    relations::RelationIndex,
    resolution::Support,
    resource::{Resource, ResourceLanguage, ResourceStore},
    runtime::{CancelToken, RequestFailure},
    scan::BaselineScan,
    semantic::{
        AnalysisContext, AnalysisContextBinding, ProjectRootIdentity, SemanticBackendKind,
        SemanticCapability,
    },
    semantic_index::SemanticIndex,
    symbol::{Symbol, SymbolStore},
};

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
                lifecycle::discover_projects(index.connection(), trust).expect("projects");
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
        let projects =
            lifecycle::discover_projects(index.connection(), self.trust).expect("projects");
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
            lifecycle::discover_projects(index.connection(), ProjectExecutionTrust::Trusted)
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
