//! Python P0 Level A acceptance, against the real pinned Pyright
//! (#19 task 9).
//!
//! What this adds to the suites that already exist: the locked P0
//! surface is checked *end to end on one Workspace*, with one backend
//! process, through the ordinary Brainprint query APIs -- not through
//! any Python-specific Agent surface, because the whole point of tasks
//! 1-8 is that `callers`, `references`, impact traversal and the
//! inspect preparer get more accurate without learning Python.
//!
//! Acceptance is against the locked P0 contract, not against "all
//! possible Python semantics". A capability that is honestly PARTIAL
//! stays PARTIAL; what has to hold is that the P0 cases answer and that
//! everything outside them stays an explicit gap rather than becoming a
//! false confirmed fact or a false zero.
//!
//! The census this prints at the end is what
//! `benchmarks/i4-python-acceptance-report.md` records. It is printed,
//! not derived twice.
//!
//! ```sh
//! cd scripts/python_semantic_spike && npm install && cd -
//! cargo test -p brainprint-engine --test i4_python_acceptance -- --ignored --nocapture
//! ```

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process,
    sync::Arc,
    time::Duration,
};

use brainprint_core::{ResourceId, SymbolId, WorkspaceId};
use brainprint_engine::{
    config::WorkspaceConfig,
    graph::{GraphEndpoint, RelationKind},
    impact::{Budget, ImpactIntent, ImpactTraversal},
    prepare::{InspectPreparer, RangeRole},
    python_semantic::{
        BatchPolicy, LeaseQueries, PyrightInstall, PythonLauncher, PythonSettings, RefreshRequest,
        capability_report, lifecycle, refresh_resource, toolchain_identity,
    },
    relations::{Direction, RelationIndex},
    resolution::Support,
    resource::{ResourceLanguage, ResourceStore},
    runtime::{RequestOptions, RuntimePolicy, SemanticRuntimeSupervisor},
    scan::BaselineScan,
    semantic::{
        AnalysisContext, AnalysisContextBinding, ProjectRootIdentity, SemanticBackendKind,
        SemanticCapability,
    },
    semantic_index::{SemanticIndex, SemanticOwner, SemanticState},
    symbol::{Symbol, SymbolStore},
};
use rusqlite::Connection;

// ---------------------------------------------------------------------
// Fixture plumbing
// ---------------------------------------------------------------------

fn install_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/python_semantic_spike")
}

fn fixture_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/workspaces/python-semantic-spike")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("destination");
    for entry in fs::read_dir(from).expect("read fixture") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy");
        }
    }
}

/// One indexed Workspace with one Pyright behind it.
struct Slice {
    workspace: PathBuf,
    db_path: PathBuf,
    base: PathBuf,
    context: AnalysisContext,
    supervisor: SemanticRuntimeSupervisor,
}

impl Slice {
    fn open(label: &str, workspace_uid: u8, install: &PyrightInstall) -> Self {
        let base = env::temp_dir().join(format!("brainprint-i4-{label}-{}", process::id()));
        let _ = fs::remove_dir_all(&base);
        let workspace = base.join("workspace");
        copy_tree(&fixture_source(), &workspace);
        let db_path = base.join("data").join("index.db");
        BaselineScan::open(&db_path)
            .expect("index.db")
            .run_initial_scan(&workspace, &WorkspaceConfig::default(), "workspace-rev-1")
            .expect("baseline scan");

        let settings = PythonSettings::default();
        let context = AnalysisContext {
            workspace: WorkspaceId::from_bytes([workspace_uid; 16]),
            backend: SemanticBackendKind::Python,
            language: ResourceLanguage::Python,
            project_root: ProjectRootIdentity::Key(format!("python-semantic-spike-{label}")),
            toolchain: toolchain_identity(
                install,
                &lifecycle::environment_identity(&workspace, &settings),
            ),
        };
        let supervisor =
            SemanticRuntimeSupervisor::new(RuntimePolicy::default()).with_backend(Arc::new(
                PythonLauncher::new(install.clone(), workspace.clone(), settings),
            ));
        Self {
            workspace,
            db_path,
            base,
            context,
            supervisor,
        }
    }

    fn binding(&self) -> AnalysisContextBinding {
        AnalysisContextBinding {
            context: self.context.clone(),
            project_root_rel: String::new(),
            config_file_rel: Some("pyrightconfig.json".to_owned()),
        }
    }

    fn resource(&self, rel: &str) -> brainprint_engine::resource::Resource {
        ResourceStore::open(&self.db_path)
            .expect("index.db")
            .list_active()
            .expect("resources")
            .into_iter()
            .find(|resource| resource.path_key == rel)
            .unwrap_or_else(|| panic!("{rel} is indexed"))
    }

    fn python_files(&self) -> Vec<String> {
        let mut found: Vec<String> = ResourceStore::open(&self.db_path)
            .expect("index.db")
            .list_active()
            .expect("resources")
            .into_iter()
            .filter(|resource| resource.language == Some(ResourceLanguage::Python))
            .map(|resource| resource.path_key)
            .collect();
        found.sort();
        found
    }

    fn symbol(&self, rel: &str, qualified_name: &str) -> Symbol {
        SymbolStore::open(&self.db_path)
            .expect("index.db")
            .list_for_resource(self.resource(rel).id)
            .expect("symbols")
            .into_iter()
            .find(|symbol| symbol.qualified_name == qualified_name)
            .unwrap_or_else(|| panic!("{qualified_name} in {rel}"))
    }

    fn outgoing(&self, from: SymbolId, kinds: &[RelationKind]) -> Vec<GraphEndpoint> {
        RelationIndex::open(&self.db_path)
            .expect("index.db")
            .outgoing(&GraphEndpoint::Symbol(from), kinds)
            .expect("outgoing")
            .confirmed
            .into_iter()
            .map(|relation| relation.target)
            .collect()
    }

    fn resource_outgoing(&self, from: ResourceId, kinds: &[RelationKind]) -> Vec<GraphEndpoint> {
        RelationIndex::open(&self.db_path)
            .expect("index.db")
            .outgoing(&GraphEndpoint::Resource(from), kinds)
            .expect("outgoing")
            .confirmed
            .into_iter()
            .map(|relation| relation.target)
            .collect()
    }

    fn count(&self, sql: &str) -> i64 {
        Connection::open(&self.db_path)
            .expect("index.db")
            .query_row(sql, [], |row| row.get(0))
            .expect("count")
    }

    fn owner(&self, rel: &str) -> SemanticOwner {
        SemanticOwner::new(self.context.context_key(), self.resource(rel).id)
    }
}

impl Drop for Slice {
    fn drop(&mut self) {
        self.supervisor.shutdown();
        let _ = fs::remove_dir_all(&self.base);
    }
}

/// Refresh every Python Resource in the slice, once.
fn refresh_everything(slice: &Slice, queries: &LeaseQueries<'_>) -> usize {
    let index = SemanticIndex::open(&slice.db_path).expect("index.db");
    let discovered = lifecycle::discover_config(index.connection(), &slice.workspace, "")
        .expect("discover config");
    assert_eq!(
        discovered.source,
        lifecycle::ConfigSource::PyrightConfig,
        "the fixture's pyrightconfig.json is what the backend uses"
    );
    let config = discovered.basis(&PythonSettings::default());
    let capabilities = capability_report(&slice.context);
    let mut evidence = 0;
    for rel in slice.python_files() {
        let owner = slice.resource(&rel);
        let outcome = refresh_resource(
            &index,
            queries,
            &RefreshRequest {
                context: &slice.context,
                workspace_root: &slice.workspace,
                owner: owner.id,
                config: &config,
                capabilities: &capabilities,
                policy: BatchPolicy { max_attempts: 12 },
            },
        )
        .unwrap_or_else(|error| panic!("{rel}: {error}"));
        evidence += outcome.evidence_count;
    }
    evidence
}

// ---------------------------------------------------------------------
// The acceptance run
// ---------------------------------------------------------------------

#[test]
#[ignore = "needs the pinned pyright-typeserver install; see the module docs"]
#[allow(clippy::too_many_lines)]
fn the_python_p0_level_a_slice_holds_against_the_real_backend() {
    let Ok(install) = PyrightInstall::locate(&install_root(), "node") else {
        println!(
            "skipped: no pinned install under {}",
            install_root().display()
        );
        return;
    };
    println!("pyright-typeserver {}", install.package_version);

    let slice = Slice::open("level-a", 21, &install);
    let lease = slice
        .supervisor
        .acquire(&slice.binding())
        .expect("the type server starts");

    // Task 2 invariant, small form: several logical callers of one
    // AnalysisContext share one project analysis. Task 14 owns the
    // full concurrency suite; this only proves the shape.
    let second = slice
        .supervisor
        .acquire(&slice.binding())
        .expect("a second caller");
    let third = slice
        .supervisor
        .acquire(&slice.binding())
        .expect("a third caller");
    assert_eq!(
        slice.supervisor.live_runtime_count(),
        1,
        "three callers, one Pyright"
    );
    drop(second);
    drop(third);

    let queries = LeaseQueries::new(
        &lease,
        RequestOptions::with_timeout(Duration::from_secs(60)),
    );
    let evidence = refresh_everything(&slice, &queries);
    assert!(evidence > 0);

    // ---- Package / module / import -------------------------------
    let imports = slice.resource("pkg/imports.py");
    let import_targets = slice.resource_outgoing(imports.id, &[RelationKind::Imports]);
    let base_resource = GraphEndpoint::Resource(slice.resource("pkg/base.py").id);
    assert!(
        import_targets.contains(&base_resource),
        "`import pkg.base` and `from .base import Base` both reach the Resource: {import_targets:?}"
    );
    assert!(
        import_targets
            .iter()
            .any(|target| matches!(target, GraphEndpoint::External(_))),
        "`import requests` is an external identity, not a Resource: {import_targets:?}"
    );
    // The re-export chain: uses.py names `Exported`, which is `Base`.
    let uses_targets = slice.resource_outgoing(slice.resource("pkg/uses.py").id, &[]);
    let reexport = GraphEndpoint::Resource(slice.resource("pkg/reexport.py").id);
    assert!(
        uses_targets.contains(&reexport),
        "the alias import binds to the module that re-exports: {uses_targets:?}"
    );
    let exported_reaches_base = slice
        .outgoing(
            slice.symbol("pkg/uses.py", "wire").id,
            &[RelationKind::References, RelationKind::UsesType],
        )
        .contains(&GraphEndpoint::Symbol(
            slice.symbol("pkg/base.py", "Base").id,
        ));
    println!("re-export chain resolves through the alias: {exported_reaches_base}");

    // ---- Definition / cross-file binding, and the same-name trap --
    // `x.run(1)` in impl.py must bind to `Base.run` by the location the
    // backend returned -- never to one of the five other `run`s.
    let base_run = slice.symbol("pkg/base.py", "Base.run");
    let same_name: Vec<Symbol> = [
        ("pkg/base.py", "Base.run"),
        ("pkg/impl.py", "Impl.run"),
        ("pkg/twin.py", "Other.run"),
        ("pkg/inherit.py", "Mixin.run"),
        ("pkg/inherit.py", "Unrelated.run"),
        ("pkg/shapes.py", "DuckTyped.run"),
    ]
    .into_iter()
    .map(|(rel, name)| slice.symbol(rel, name))
    .collect();
    assert_eq!(same_name.len(), 6, "the trap needs every same-name symbol");

    let call_targets = slice.outgoing(
        slice.symbol("pkg/impl.py", "call").id,
        &[RelationKind::Calls],
    );
    assert_eq!(
        call_targets,
        vec![GraphEndpoint::Symbol(base_run.id)],
        "the receiver-typed call chose exactly one of six `run`s"
    );

    // And it is anchored on the real call site, not on a name match.
    let impl_source = fs::read_to_string(slice.workspace.join("pkg/impl.py")).expect("source");
    let anchored = slice.count(
        "SELECT COUNT(*) FROM semantic_evidence \
             JOIN occurrence ON occurrence.id = semantic_evidence.occurrence_id \
             WHERE semantic_evidence.capability IN ('CALLS_CROSS_FILE','CALLS_INTRA_FILE')",
    );
    assert!(anchored > 0, "call evidence is anchored on occurrences");
    let (start, end): (i64, i64) = Connection::open(&slice.db_path)
        .expect("index.db")
        .query_row(
            "SELECT occurrence.start_byte, occurrence.end_byte \
             FROM semantic_evidence \
             JOIN occurrence ON occurrence.id = semantic_evidence.occurrence_id \
             WHERE semantic_evidence.capability LIKE 'CALLS%' \
               AND occurrence.containing_symbol_id = \
                   (SELECT id FROM symbol WHERE uid = ?1)",
            rusqlite::params![slice.symbol("pkg/impl.py", "call").id.to_bytes().to_vec()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("the call evidence row");
    let slice_of =
        &impl_source[usize::try_from(start).expect("fits")..usize::try_from(end).expect("fits")];
    // The CALL_SITE occurrence spans the callee expression as written,
    // which is what an Agent has to edit. (The OVERRIDES evidence
    // anchors the declaration's name token instead; both are exact,
    // and neither is a whole-line or whole-file approximation.)
    assert_eq!(slice_of, "x.run", "anchored on the exact call site");

    // ---- Inheritance ---------------------------------------------
    let base = GraphEndpoint::Symbol(slice.symbol("pkg/base.py", "Base").id);
    let mixin = GraphEndpoint::Symbol(slice.symbol("pkg/inherit.py", "Mixin").id);
    for (rel, name, expected) in [
        ("pkg/impl.py", "Impl", vec![base.clone()]),
        ("pkg/inherit.py", "Qualified", vec![base.clone()]),
        ("pkg/inherit.py", "Outer.Inner", vec![mixin.clone()]),
    ] {
        assert_eq!(
            slice.outgoing(slice.symbol(rel, name).id, &[RelationKind::Extends]),
            expected,
            "{name}"
        );
    }
    let external_base = slice.outgoing(
        slice.symbol("pkg/shapes.py", "Abstract").id,
        &[RelationKind::Extends],
    );
    assert_eq!(external_base.len(), 1);
    assert!(
        matches!(external_base[0], GraphEndpoint::External(_)),
        "`abc.ABC` is an external identity: {external_base:?}"
    );
    let multi = slice.outgoing(
        slice.symbol("pkg/inherit.py", "Multi").id,
        &[RelationKind::Extends],
    );
    assert_eq!(multi.len(), 2, "both bases of a multiple inheritance");

    // ---- Overrides ------------------------------------------------
    for (rel, name, expected) in [
        ("pkg/impl.py", "Impl.run", 1),
        ("pkg/inherit.py", "Qualified.run", 1),
        ("pkg/inherit.py", "OnlyOne.only_mixin", 1),
        ("pkg/inherit.py", "Declared.run", 1),
        ("pkg/shapes.py", "Concrete.compute", 1),
        ("pkg/shapes.py", "Concrete.build", 1),
        ("pkg/shapes.py", "Concrete.helper", 1),
        ("pkg/shapes.py", "Concrete.label", 1),
        ("pkg/inherit.py", "Outer.Inner.run", 1),
        // Traps: no ancestor declares these, or two same-depth
        // ancestors do and the graph cannot prove which wins.
        ("pkg/twin.py", "Other.run", 0),
        ("pkg/inherit.py", "Unrelated.run", 0),
        ("pkg/inherit.py", "Multi.run", 0),
        ("pkg/inherit.py", "Orphan.run", 0),
        ("pkg/shapes.py", "DuckTyped.run", 0),
    ] {
        assert_eq!(
            slice
                .outgoing(slice.symbol(rel, name).id, &[RelationKind::Overrides])
                .len(),
            expected,
            "{name}"
        );
    }

    // ---- Implements: never manufactured from a matching shape -----
    assert_eq!(
        slice.count("SELECT COUNT(*) FROM relation WHERE kind = 'IMPLEMENTS'"),
        0,
        "duck typing is not evidence"
    );

    // ---- Types ----------------------------------------------------
    let plain_types = slice.outgoing(
        slice.symbol("pkg/shapes.py", "plain").id,
        &[RelationKind::UsesType],
    );
    assert!(
        plain_types.contains(&GraphEndpoint::Symbol(
            slice.symbol("pkg/shapes.py", "Model").id
        )),
        "an annotated parameter binds to the declared type: {plain_types:?}"
    );
    assert!(
        plain_types.contains(&base),
        "and to a cross-file one: {plain_types:?}"
    );

    // ---- Dynamic Python: a gap, never a confirmed edge ------------
    let dispatch = slice.symbol("pkg/dynamic.py", "dispatch");
    let dynamic_calls = slice.outgoing(dispatch.id, &[RelationKind::Calls]);
    assert!(
        !dynamic_calls.contains(&GraphEndpoint::Symbol(base_run.id)),
        "`getattr(target, name)()` must not become a static call to Base.run: {dynamic_calls:?}"
    );
    assert!(
        dynamic_calls
            .iter()
            .all(|target| matches!(target, GraphEndpoint::External(_))),
        "the only confirmed call in `dispatch` is `getattr` itself, an external \
         builtin; the attribute it returns is not a static target: {dynamic_calls:?}"
    );
    println!(
        "dynamic dispatch confirmed targets: {}",
        dynamic_calls.len()
    );

    // ---- External dependency boundary -----------------------------
    let resources = ResourceStore::open(&slice.db_path)
        .expect("index.db")
        .list_active()
        .expect("resources");
    assert!(
        resources.iter().all(|resource| {
            !resource.path_key.contains("site-packages")
                && !resource.path_key.contains("typeshed")
                && !resource.path_key.contains("node_modules")
        }),
        "no dependency file became a Resource"
    );
    let resource_rows = resources.len();

    // ---- One logical relation, not one per tier -------------------
    assert_eq!(
        slice.count(
            "SELECT COUNT(*) FROM (SELECT kind, source_entity_id, target_entity_id, \
             COUNT(*) AS n FROM relation \
             GROUP BY kind, source_entity_id, target_entity_id HAVING n > 1)"
        ),
        0,
        "structural and semantic proof of one fact is one row"
    );
    assert_eq!(
        slice.count(
            "SELECT COUNT(*) FROM (SELECT resource_id, kind, start_byte, end_byte, \
             COUNT(*) AS n FROM occurrence \
             GROUP BY resource_id, kind, start_byte, end_byte HAVING n > 1)"
        ),
        0,
        "and one site is one Occurrence"
    );

    // ---- The Agent-facing surfaces --------------------------------
    // Impact from the base member reaches the overriding declaration.
    let impact = ImpactTraversal::open(&slice.db_path)
        .expect("index.db")
        .run(
            ImpactIntent::PublicSignatureChange,
            &GraphEndpoint::Symbol(base_run.id),
            &Budget::default(),
        )
        .expect("impact");
    assert!(
        impact.nodes.iter().any(|node| node.endpoint
            == GraphEndpoint::Symbol(slice.symbol("pkg/impl.py", "Impl.run").id)),
        "a signature change on Base.run reaches Impl.run"
    );
    let base_impact = ImpactTraversal::open(&slice.db_path)
        .expect("index.db")
        .run(ImpactIntent::BaseInterfaceChange, &base, &Budget::default())
        .expect("impact");
    assert!(
        base_impact
            .nodes
            .iter()
            .any(|node| node.endpoint
                == GraphEndpoint::Symbol(slice.symbol("pkg/impl.py", "Impl").id)),
        "and a base change reaches the subclass"
    );

    // Prepared inspection: current source, not a line number to go and
    // read. This is a hard gate -- a location-only answer where the
    // preparer could have sliced source is an acceptance failure.
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
        for evidence in &relation.evidence {
            assert!(
                evidence.evidence_range.is_some(),
                "a semantic result with only a location: {:?}",
                evidence.location
            );
            assert!(evidence.unavailable.is_none(), "{:?}", evidence.unavailable);
        }
    }
    assert!(
        prepared
            .ranges
            .iter()
            .any(|range| range.role == RangeRole::ContainingDeclaration
                && range.source.contains("def ")),
        "the declaration a call sits in comes back as source"
    );
    let prepared_bytes: usize = prepared.ranges.iter().map(|range| range.source.len()).sum();

    // ---- Freshness, on the real backend ---------------------------
    let index = SemanticIndex::open(&slice.db_path).expect("index.db");
    let impl_owner = slice.owner("pkg/impl.py");
    assert_eq!(
        index.status(&impl_owner).expect("status").state,
        SemanticState::Current
    );
    // A dependency moves and the dependent stops being current --
    // before any refresh, and without impl.py being touched.
    let touched = index
        .invalidate_resource(slice.resource("pkg/base.py").id)
        .expect("invalidate");
    assert!(touched.contains(&impl_owner));
    assert_ne!(
        index.status(&impl_owner).expect("status").state,
        SemanticState::Current,
        "a stale semantic proof may not read CURRENT"
    );
    assert!(
        index.status(&impl_owner).expect("status").has_last_valid(),
        "the last valid publication is kept, never replaced with an empty success"
    );

    // ---- The matrix has to match what just happened ---------------
    // A declaration nobody measured is a claim, and a measurement
    // nobody declared is coverage an Agent is never told about. Both
    // are matrix defects, so both are asserted here against the same
    // run that produced the relations above.
    let declared = capability_report(&slice.context);
    for (capability, expected) in [
        (SemanticCapability::ImportBinding, Support::Supported),
        (SemanticCapability::AliasResolution, Support::Supported),
        (SemanticCapability::ReexportResolution, Support::Supported),
        (SemanticCapability::SymbolDefinition, Support::Supported),
        (SemanticCapability::References, Support::Supported),
        (SemanticCapability::CallsIntraFile, Support::Supported),
        (SemanticCapability::CallsCrossFile, Support::Supported),
        (
            SemanticCapability::ExternalSymbolResolution,
            Support::Supported,
        ),
        (SemanticCapability::TypeResolution, Support::Partial),
        (SemanticCapability::Inheritance, Support::Partial),
        (SemanticCapability::Overrides, Support::Partial),
        (SemanticCapability::StaticDispatchTarget, Support::Partial),
        (SemanticCapability::Implements, Support::Unsupported),
        (
            SemanticCapability::ImplementationTarget,
            Support::Unsupported,
        ),
        (SemanticCapability::OverloadResolution, Support::Unsupported),
    ] {
        assert_eq!(declared.support(capability), expected, "{capability:?}");
    }
    // The structural capabilities are I2/I3's, and this backend does
    // not claim them. An empty semantic answer for one is a statement
    // about who owns the fact, not about the code.
    for structural in [
        SemanticCapability::ResourceDiscovery,
        SemanticCapability::SyntaxStructure,
        SemanticCapability::SymbolSpan,
        SemanticCapability::ContainingScope,
        SemanticCapability::ImportDeclaration,
        SemanticCapability::ExportDeclaration,
        SemanticCapability::EmbeddedRegionMapping,
        SemanticCapability::OriginalSourceMapping,
    ] {
        assert_eq!(
            declared.support(structural),
            Support::Unsupported,
            "{structural:?} is provided structurally, and this report says so"
        );
    }
    // A declared-UNSUPPORTED capability produced nothing, which is the
    // other half of the same contract.
    assert_eq!(
        slice.count("SELECT COUNT(*) FROM relation WHERE kind = 'IMPLEMENTS'"),
        0
    );
    // STATIC is claimed only where it is one: a receiver-typed call is
    // recorded UNKNOWN rather than mislabelled.
    assert!(
        slice.count("SELECT COUNT(*) FROM relation WHERE kind = 'CALLS' AND dispatch = 'UNKNOWN'")
            > 0,
        "virtual dispatch must not be recorded STATIC"
    );

    // ---- The census the report records ----------------------------
    let census = census(&slice);
    println!("--- i4-python-acceptance census ---");
    {
        let connection = Connection::open(&slice.db_path).expect("index.db");
        let mut statement = connection
            .prepare(
                "SELECT kind, COALESCE(dispatch, '-'), COUNT(*) FROM relation \
                 GROUP BY kind, dispatch ORDER BY kind, dispatch",
            )
            .expect("prepare");
        let rows = statement
            .query_map([], |row| {
                Ok(format!(
                    "dispatch_{}_{}={}",
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?
                ))
            })
            .expect("query");
        for row in rows {
            println!("{}", row.expect("dispatch row"));
        }
    }
    for (reason, count) in unresolved_by_reason(&slice) {
        println!("unresolved_reason_{reason}={count}");
    }
    for (key, value) in &census {
        println!("{key}={value}");
    }
    println!("resource_rows={resource_rows}");
    println!("prepared_source_bytes={prepared_bytes}");
    println!("prepared_ranges={}", prepared.ranges.len());
    println!("semantic_evidence_total={evidence}");
    println!("--- end census ---");
}

/// What is still unresolved, and why. An acceptance report that says
/// "7 gaps" without saying which kinds is not saying anything.
fn unresolved_by_reason(slice: &Slice) -> Vec<(String, i64)> {
    let connection = Connection::open(&slice.db_path).expect("index.db");
    let mut statement = connection
        .prepare(
            "SELECT reason, COUNT(*) FROM unresolved_reference \
             GROUP BY reason ORDER BY reason",
        )
        .expect("prepare");
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query");
    rows.collect::<Result<_, _>>().expect("reasons")
}

/// The graph's shape, as counts. Printed so the report quotes measured
/// values rather than remembered ones.
fn census(slice: &Slice) -> BTreeMap<String, i64> {
    let mut found = BTreeMap::new();
    for kind in [
        "CALLS",
        "EXTENDS",
        "IMPLEMENTS",
        "IMPORTS",
        "OVERRIDES",
        "REFERENCES",
        "USES_TYPE",
    ] {
        found.insert(
            format!("relation_{}", kind.to_lowercase()),
            slice.count(&format!(
                "SELECT COUNT(*) FROM relation WHERE kind = '{kind}'"
            )),
        );
    }
    found.insert(
        "relation_total".to_owned(),
        slice.count("SELECT COUNT(*) FROM relation"),
    );
    found.insert(
        "unresolved_total".to_owned(),
        slice.count("SELECT COUNT(*) FROM unresolved_reference"),
    );
    found.insert(
        "unresolved_requires_semantics".to_owned(),
        slice.count(
            "SELECT COUNT(*) FROM unresolved_reference WHERE reason IN \
             ('MODULE_TREE_REQUIRES_SEMANTICS','NAMESPACE_REQUIRES_SEMANTICS',\
              'RECEIVER_TYPE_REQUIRED','TYPE_SEMANTICS_REQUIRED',\
              'OVERRIDE_TARGET_REQUIRES_SEMANTICS','CONFIG_DEPENDENT_SPECIFIER')",
        ),
    );
    found.insert(
        "relation_candidates".to_owned(),
        slice.count("SELECT COUNT(*) FROM relation_candidate"),
    );
    found.insert(
        "semantic_evidence_rows".to_owned(),
        slice.count("SELECT COUNT(*) FROM semantic_evidence"),
    );
    found.insert(
        "semantic_publications".to_owned(),
        slice.count("SELECT COUNT(*) FROM semantic_publication"),
    );
    found
}

// ---------------------------------------------------------------------
// Worktree isolation, with two real backends
// ---------------------------------------------------------------------

#[test]
#[ignore = "needs the pinned pyright-typeserver install; see the module docs"]
fn two_worktrees_of_the_same_fixture_never_share_semantic_state() {
    let Ok(install) = PyrightInstall::locate(&install_root(), "node") else {
        println!("skipped: no pinned install");
        return;
    };

    let left = Slice::open("worktree-a", 31, &install);
    let right = Slice::open("worktree-b", 32, &install);
    assert_ne!(
        left.context.context_key(),
        right.context.context_key(),
        "the same relative paths in two Workspaces are two contexts"
    );

    let lease = left.supervisor.acquire(&left.binding()).expect("left");
    let queries = LeaseQueries::new(
        &lease,
        RequestOptions::with_timeout(Duration::from_secs(60)),
    );
    refresh_everything(&left, &queries);
    drop(lease);

    // The left worktree is fully current. The right one has never been
    // analyzed, and must say so.
    let left_index = SemanticIndex::open(&left.db_path).expect("index.db");
    let right_index = SemanticIndex::open(&right.db_path).expect("index.db");
    assert_eq!(
        left_index
            .status(&left.owner("pkg/impl.py"))
            .expect("status")
            .state,
        SemanticState::Current
    );
    assert_eq!(
        right_index
            .status(&right.owner("pkg/impl.py"))
            .expect("status")
            .state,
        SemanticState::None,
        "one worktree's publication is not the other's"
    );
    assert!(
        right_index
            .owners_of_context(&right.context.context_key())
            .expect("owners")
            .is_empty()
    );
    // Cross-reading with the other context's key finds nothing either.
    assert_eq!(
        right_index
            .status(&SemanticOwner::new(
                left.context.context_key(),
                right.resource("pkg/impl.py").id
            ))
            .expect("status")
            .state,
        SemanticState::None
    );

    assert_eq!(
        left.supervisor.live_runtime_count(),
        1,
        "and one worktree's runtime is its own"
    );
    assert_eq!(right.supervisor.live_runtime_count(), 0);
}

// ---------------------------------------------------------------------
// Level B: the same Workspace with no backend at all
// ---------------------------------------------------------------------

/// Runs everywhere, on purpose. The claim it protects is that the
/// structural workflow does not depend on anyone having Pyright.
#[test]
fn the_structural_workflow_is_unchanged_with_no_backend_installed() {
    let base = env::temp_dir().join(format!("brainprint-i4-levelb-{}", process::id()));
    let _ = fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    copy_tree(&fixture_source(), &workspace);
    let db_path = base.join("data").join("index.db");
    BaselineScan::open(&db_path)
        .expect("index.db")
        .run_initial_scan(&workspace, &WorkspaceConfig::default(), "workspace-rev-1")
        .expect("baseline scan");

    // Nothing is started, and nothing may be.
    let supervisor = SemanticRuntimeSupervisor::new(RuntimePolicy::default());
    assert_eq!(supervisor.live_runtime_count(), 0);

    let store = ResourceStore::open(&db_path).expect("index.db");
    let resources = store.list_active().expect("resources");
    assert!(resources.len() >= 12, "Resources: {}", resources.len());

    let impl_resource = resources
        .iter()
        .find(|resource| resource.path_key == "pkg/impl.py")
        .expect("pkg/impl.py");
    let symbols = SymbolStore::open(&db_path)
        .expect("index.db")
        .list_for_resource(impl_resource.id)
        .expect("symbols");
    assert!(
        symbols
            .iter()
            .any(|symbol| symbol.qualified_name == "Impl.run"),
        "Symbols survive a missing backend"
    );

    let relations = RelationIndex::open(&db_path).expect("index.db");
    let structural = relations
        .outgoing(&GraphEndpoint::Resource(impl_resource.id), &[])
        .expect("outgoing");
    assert!(
        !structural.confirmed.is_empty(),
        "structural relations survive a missing backend"
    );

    // The semantic-required questions are gaps, not zeros.
    let connection = Connection::open(&db_path).expect("index.db");
    let requires_semantics: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM unresolved_reference WHERE reason IN \
             ('MODULE_TREE_REQUIRES_SEMANTICS','NAMESPACE_REQUIRES_SEMANTICS',\
              'RECEIVER_TYPE_REQUIRED','TYPE_SEMANTICS_REQUIRED',\
              'OVERRIDE_TARGET_REQUIRES_SEMANTICS','CONFIG_DEPENDENT_SPECIFIER')",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert!(
        requires_semantics > 0,
        "a Level A question with no backend is an explicit gap"
    );
    let publications: i64 = connection
        .query_row("SELECT COUNT(*) FROM semantic_publication", [], |row| {
            row.get(0)
        })
        .expect("count");
    assert_eq!(
        publications, 0,
        "and nothing was published as an empty success"
    );

    // The preparer still hands back current source for what is proven.
    let base_symbol = SymbolStore::open(&db_path)
        .expect("index.db")
        .list_for_resource(
            resources
                .iter()
                .find(|resource| resource.path_key == "pkg/base.py")
                .expect("pkg/base.py")
                .id,
        )
        .expect("symbols")
        .into_iter()
        .find(|symbol| symbol.qualified_name == "Base")
        .expect("Base");
    let prepared = InspectPreparer::open(&db_path, &workspace)
        .expect("preparer")
        .prepare(
            &GraphEndpoint::Symbol(base_symbol.id),
            Direction::Incoming,
            &[RelationKind::Extends],
        )
        .expect("prepared");
    assert!(prepared.source_complete());

    let kinds: BTreeSet<String> = connection
        .prepare("SELECT DISTINCT kind FROM relation")
        .expect("prepare")
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("kinds");
    println!("level-b relation kinds: {kinds:?}");
    println!("level-b requires_semantics gaps: {requires_semantics}");

    let _ = fs::remove_dir_all(&base);
}
