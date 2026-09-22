//! TypeScript/JavaScript P0 Level A acceptance, against the real
//! pinned TypeScript 7 native language server (#19 task 10).
//!
//! What this adds to the transport suite that already exists: the
//! locked P0 surface is checked *end to end on one Workspace*, with one
//! backend process, through the ordinary Brainprint query APIs -- not
//! through any TypeScript-specific Agent surface, because the whole
//! point of tasks 1-10 is that `callers`, `references`, impact
//! traversal and the inspect preparer get more accurate without
//! learning TypeScript.
//!
//! Every fact here travels the whole tier:
//!
//! ```text
//! LSP → adapter → SemanticEvidence → publication → merge → query API
//! ```
//!
//! A transport measurement is not acceptance; that is what
//! `typescript_semantic_lsp.rs` is for, and it stays.
//!
//! ```sh
//! cd scripts/typescript_semantic_spike && npm install && cd -
//! cargo test -p brainprint-engine --test i4_typescript_acceptance -- --ignored --nocapture
//! ```

use std::{
    env, fs,
    path::{Path, PathBuf},
    process,
    sync::Arc,
};

use brainprint_core::{ResourceId, SymbolId, WorkspaceId};
use brainprint_engine::{
    config::WorkspaceConfig,
    graph::{GraphEndpoint, RelationKind},
    impact::{Budget, ImpactIntent, ImpactTraversal},
    prepare::InspectPreparer,
    related_tests::RelatedTests,
    relations::{Direction, RelationIndex},
    resolution::Support,
    resource::{Resource, ResourceLanguage, ResourceStore},
    runtime::{
        CancelToken, RequestFailure, RuntimePolicy, SemanticBackendLauncher,
        SemanticRuntimeSupervisor,
    },
    scan::BaselineScan,
    semantic::{
        AnalysisContext, AnalysisContextBinding, ProjectRootIdentity, SemanticBackendKind,
        SemanticCapability,
    },
    semantic_index::{SemanticIndex, SemanticOwner, SemanticState},
    symbol::{Symbol, SymbolStore},
    typescript_semantic::{
        RefreshRequest, TypeScriptHost, TypeScriptInstall, TypeScriptLauncher, TypeScriptQueries,
        capability_report, javascript_capability_report, lifecycle,
        protocol::{TypeScriptRequest, TypeScriptResponse},
        refresh_resource, toolchain_identity,
    },
};
use rusqlite::Connection;

// ---------------------------------------------------------------------
// Fixture plumbing
// ---------------------------------------------------------------------

fn install_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/typescript_semantic_spike")
}

fn fixture_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/workspaces/typescript-semantic-spike")
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

/// Asks the live host directly.
///
/// The supervisor path is exercised too (see the process-count
/// assertion), but the adapter only needs *something* that answers, and
/// driving the host keeps a 20-file refresh from paying the queue's
/// round trip twenty times over.
struct HostQueries<'a> {
    host: &'a TypeScriptHost,
    cancel: CancelToken,
}

impl TypeScriptQueries for HostQueries<'_> {
    fn call(&self, request: &TypeScriptRequest) -> Result<TypeScriptResponse, RequestFailure> {
        self.host
            .call(request, &self.cancel)
            .map_err(RequestFailure::Backend)
    }
}

/// One indexed Workspace with one TypeScript server behind it.
struct Slice {
    workspace: PathBuf,
    db_path: PathBuf,
    base: PathBuf,
    context: AnalysisContext,
    launcher: Arc<TypeScriptLauncher>,
}

impl Slice {
    fn open(label: &str, workspace_uid: u8, install: &TypeScriptInstall) -> Self {
        let base = env::temp_dir().join(format!("brainprint-i4ts-{label}-{}", process::id()));
        let _ = fs::remove_dir_all(&base);
        let workspace = base.join("workspace");
        copy_tree(&fixture_source(), &workspace);
        // The *pinned* dependency tree, linked rather than installed
        // per test: that is what makes "an external package resolves to
        // an identity and its source is never indexed" a claim about a
        // real `node_modules`.
        let pinned = install_root()
            .join("node_modules")
            .canonicalize()
            .expect("pinned node_modules");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&pinned, workspace.join("node_modules")).expect("link");
        #[cfg(not(unix))]
        copy_tree(&pinned, &workspace.join("node_modules"));

        let db_path = base.join("data").join("index.db");
        BaselineScan::open(&db_path)
            .expect("index.db")
            .run_initial_scan(&workspace, &WorkspaceConfig::default(), "workspace-rev-1")
            .expect("baseline scan");

        let environment = {
            let index = SemanticIndex::open(&db_path).expect("index.db");
            lifecycle::environment_identity(
                index.connection(),
                &workspace,
                "",
                &install.manifest_version,
            )
            .expect("environment")
        };
        let context = AnalysisContext {
            workspace: WorkspaceId::from_bytes([workspace_uid; 16]),
            backend: SemanticBackendKind::TypeScriptJavaScript,
            language: ResourceLanguage::TypeScript,
            project_root: ProjectRootIdentity::Key(format!("typescript-spike-{label}")),
            toolchain: toolchain_identity(install, &environment),
        };
        Self {
            workspace,
            db_path,
            base,
            context,
            launcher: Arc::new(TypeScriptLauncher::new(install.clone())),
        }
    }

    fn binding(&self) -> AnalysisContextBinding {
        AnalysisContextBinding {
            context: self.context.clone(),
            project_root_rel: self.workspace.to_string_lossy().into_owned(),
            config_file_rel: Some("tsconfig.json".to_owned()),
        }
    }

    /// Re-index the Workspace, which is how a test moves it forward.
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

    fn served_files(&self) -> Vec<String> {
        let mut found: Vec<String> = self
            .resources()
            .into_iter()
            .filter(|resource| {
                resource
                    .language
                    .is_some_and(|language| lifecycle::SERVED_LANGUAGES.contains(&language))
            })
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

    /// The Symbol whose declaration starts at `start_byte`.
    ///
    /// By span, never by name: `overload.ts` declares three functions
    /// called `parse` and the whole point is telling them apart.
    fn symbol_at(&self, rel: &str, start_byte: usize) -> Symbol {
        SymbolStore::open(&self.db_path)
            .expect("index.db")
            .list_for_resource(self.resource(rel).id)
            .expect("symbols")
            .into_iter()
            .find(|symbol| symbol.span.start_byte == start_byte)
            .unwrap_or_else(|| panic!("no symbol at {rel}:{start_byte}"))
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
        let _ = fs::remove_dir_all(&self.base);
    }
}

/// Refresh every TypeScript and JavaScript Resource in the slice, once.
fn refresh_everything(
    slice: &Slice,
    queries: &dyn TypeScriptQueries,
    encoding_known: bool,
) -> usize {
    assert!(
        encoding_known,
        "the handshake must have settled an encoding"
    );
    let index = SemanticIndex::open(&slice.db_path).expect("index.db");
    let discovered = lifecycle::discover_config(index.connection(), &slice.workspace, "")
        .expect("discover config");
    assert_eq!(discovered.source, lifecycle::ConfigSource::TsConfig);
    assert!(
        discovered.chain.len() >= 2,
        "tsconfig.json extends tsconfig.base.json, so the chain has both: {:?}",
        discovered
            .chain
            .iter()
            .map(|file| file.resource.path_key.clone())
            .collect::<Vec<_>>()
    );
    assert!(discovered.is_complete(), "{:?}", discovered.limits);
    let config = discovered.basis();
    let capabilities = capability_report(&slice.context);
    let encoding = slice
        .launcher
        .negotiated_encoding()
        .expect("the handshake settled an encoding");

    let mut evidence = 0;
    for rel in slice.served_files() {
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
                encoding,
            },
        )
        .unwrap_or_else(|error| panic!("{rel}: {error}"));
        evidence += outcome.evidence_count;
        println!("--- {rel}: {} facts", outcome.evidence_count);
        for line in &outcome.report {
            println!("      {line}");
        }
        for retained in &outcome.retained_external {
            println!(
                "      RETAINED {:?} @{}..{}",
                retained.kind, retained.occurrence.start_byte, retained.occurrence.end_byte
            );
        }
        for unproven in &outcome.unproven_overrides {
            println!("      UNPROVEN OVERRIDE {:?}", unproven.reason);
        }
        println!(
            "      merged: +{} edges, {} gaps closed, {} corroborated, {} conflicts",
            outcome.merged.relations_created,
            outcome.merged.gaps_resolved,
            outcome.merged.corroborated,
            outcome.merged.conflicts
        );
    }
    evidence
}

// ---------------------------------------------------------------------
// The acceptance run
// ---------------------------------------------------------------------

#[test]
#[ignore = "needs the pinned typescript install; see the module docs"]
#[allow(clippy::too_many_lines)]
fn the_typescript_p0_level_a_slice_holds_against_the_real_backend() {
    let Ok(install) = TypeScriptInstall::locate(&install_root()) else {
        println!(
            "skipped: no pinned install under {}",
            install_root().display()
        );
        return;
    };
    println!("typescript {}", install.manifest_version);

    let slice = Slice::open("level-a", 31, &install);
    let host = slice
        .launcher
        .start(&slice.binding())
        .expect("server starts");
    let encoding = host.encoding();
    println!(
        "server {} {} · encoding {} (negotiated: {})",
        host.server_name(),
        host.server_version(),
        encoding.encoding().as_str(),
        encoding.is_negotiated()
    );
    let queries = HostQueries {
        host: &host,
        cancel: CancelToken::new(),
    };

    let evidence = refresh_everything(&slice, &queries, true);
    assert!(evidence > 0);

    // ---- Modules: relative, alias, re-export, star, path alias ----
    let consumer = slice.resource("src/consumer.ts");
    let imports = slice.resource_outgoing(consumer.id, &[RelationKind::Imports]);
    assert!(
        imports.contains(&GraphEndpoint::Resource(slice.resource("src/public.ts").id)),
        "`./public.js` is the extension TypeScript requires you to write \
         and the file it is not: {imports:?}"
    );

    let model = GraphEndpoint::Symbol(slice.symbol("src/model.ts", "Model").id);
    let consumer_bindings = slice.resource_outgoing(
        consumer.id,
        &[RelationKind::Imports, RelationKind::References],
    );
    assert!(
        consumer_bindings.contains(&model),
        "`import {{ PublicModel }}` follows the rename *and* the re-export \
         to `Model`: {consumer_bindings:?}"
    );
    assert!(
        consumer_bindings.contains(&GraphEndpoint::Symbol(
            slice.symbol("src/core/types.ts", "Id").id
        )),
        "`import type {{ Id }} from \"./public.js\"` comes through \
         `export * from`: {consumer_bindings:?}"
    );
    let service = slice.symbol("src/core/service.ts", "Service");
    assert!(
        consumer_bindings.contains(&GraphEndpoint::Symbol(service.id)),
        "the `@core/*` path alias reaches the class the backend proves, \
         with no textual rewriting here: {consumer_bindings:?}"
    );

    // ---- The same-name trap ---------------------------------------
    // `invoke(b: Beta) {{ b.run() }}` must choose one of six `run`s.
    let same_name: Vec<Symbol> = [
        ("src/hierarchy.ts", 72),
        ("src/hierarchy.ts", 146),
        ("src/hierarchy.ts", 250),
        ("src/hierarchy.ts", 287),
        ("src/hierarchy.ts", 325),
        ("src/core/service.ts", 133),
    ]
    .into_iter()
    .map(|(rel, at)| slice.symbol_at(rel, at))
    .collect();
    assert!(
        same_name.iter().all(|symbol| symbol.name == "run"),
        "the trap needs six same-name declarations: {:?}",
        same_name.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    let beta_run = slice.symbol("src/hierarchy.ts", "Beta.run");
    assert_eq!(
        slice.outgoing(
            slice.symbol("src/hierarchy.ts", "invoke").id,
            &[RelationKind::Calls]
        ),
        vec![GraphEndpoint::Symbol(beta_run.id)],
        "the receiver-typed call chose exactly one of six `run`s"
    );
    // And it is anchored on the exact call site, not on a name match.
    let hierarchy_source =
        fs::read_to_string(slice.workspace.join("src/hierarchy.ts")).expect("source");
    let (start, end): (i64, i64) = Connection::open(&slice.db_path)
        .expect("index.db")
        .query_row(
            "SELECT occurrence.start_byte, occurrence.end_byte FROM semantic_evidence \
             JOIN occurrence ON occurrence.id = semantic_evidence.occurrence_id \
             WHERE semantic_evidence.capability LIKE 'CALLS%' \
               AND occurrence.containing_symbol_id = (SELECT id FROM symbol WHERE uid = ?1)",
            rusqlite::params![
                slice
                    .symbol("src/hierarchy.ts", "invoke")
                    .id
                    .to_bytes()
                    .to_vec()
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("the call evidence row");
    assert_eq!(
        &hierarchy_source
            [usize::try_from(start).expect("fits")..usize::try_from(end).expect("fits")],
        "b.run",
        "anchored on the exact call site"
    );

    // ---- References and calls -------------------------------------
    let esm_value = GraphEndpoint::Symbol(slice.symbol("js/esm.js", "esmValue").id);
    let total = slice.symbol("js/use-esm.js", "total");
    assert!(
        slice
            .outgoing(total.id, &[RelationKind::References])
            .contains(&esm_value),
        "a cross-file JavaScript reference"
    );
    assert!(
        slice
            .outgoing(total.id, &[RelationKind::Calls])
            .contains(&GraphEndpoint::Symbol(
                slice.symbol("js/esm.js", "esmFn").id
            )),
        "a direct cross-file JavaScript call"
    );
    assert!(
        slice
            .outgoing(
                slice.symbol("tests/service.test.ts", "testService").id,
                &[RelationKind::Calls]
            )
            .contains(&GraphEndpoint::Symbol(
                slice.symbol("src/consumer.ts", "consume").id
            )),
        "a cross-file TypeScript call"
    );
    assert!(
        slice
            .outgoing(
                slice.symbol("js/use-commonjs.cjs", "useCommonJs").id,
                &[RelationKind::Calls]
            )
            .contains(&GraphEndpoint::Symbol(
                slice.symbol("js/commonjs.cjs", "joinAll").id
            )),
        "CommonJS `require` + `module.exports` binds across files"
    );

    // ---- Dispatch is honest ---------------------------------------
    assert!(
        slice.count("SELECT COUNT(*) FROM relation WHERE kind = 'CALLS' AND dispatch = 'UNKNOWN'")
            > 0,
        "a call through a typed receiver is a declaration, not a proven \
         runtime target, and is recorded UNKNOWN"
    );

    // ---- Types ----------------------------------------------------
    let id_alias = GraphEndpoint::Symbol(slice.symbol("src/core/types.ts", "Id").id);
    assert!(
        slice
            .outgoing(
                slice
                    .symbol("src/core/service.ts", "Service.constructor")
                    .id,
                &[RelationKind::UsesType]
            )
            .contains(&id_alias),
        "an explicit annotation binds to the type alias it names"
    );
    assert!(
        slice
            .outgoing(
                slice.symbol("src/component.tsx", "Widget").id,
                &[RelationKind::UsesType]
            )
            .contains(&GraphEndpoint::Symbol(
                slice.symbol("src/component.tsx", "Props").id
            )),
        "a TSX component's props type is an ordinary USES_TYPE, with no \
         React knowledge anywhere in this tier"
    );

    // ---- Overloads ------------------------------------------------
    // Three declarations named `parse`: two signatures and the
    // implementation. The call sites must pick the signature the server
    // selected, and never the implementation.
    let string_overload = slice.symbol_at("src/overload.ts", 96);
    let number_overload = slice.symbol_at("src/overload.ts", 148);
    let implementation = slice.symbol_at("src/overload.ts", 200);
    for symbol in [&string_overload, &number_overload, &implementation] {
        assert_eq!(symbol.name, "parse");
    }
    let x_targets = slice.outgoing(
        slice.symbol("src/overload.ts", "x").id,
        &[RelationKind::Calls],
    );
    let y_targets = slice.outgoing(
        slice.symbol("src/overload.ts", "y").id,
        &[RelationKind::Calls],
    );
    assert_eq!(
        x_targets,
        vec![GraphEndpoint::Symbol(string_overload.id)],
        "`parse(\"a\")` selects the string overload"
    );
    assert_eq!(
        y_targets,
        vec![GraphEndpoint::Symbol(number_overload.id)],
        "`parse(1)` selects the number overload"
    );
    assert!(
        !x_targets.contains(&GraphEndpoint::Symbol(implementation.id))
            && !y_targets.contains(&GraphEndpoint::Symbol(implementation.id)),
        "the implementation signature is the one declaration the server \
         never selects, and it must not be mistaken for a selected overload"
    );

    // ---- Inheritance, implements, overrides -----------------------
    let base = slice.symbol("src/hierarchy.ts", "Base");
    let child = slice.symbol("src/hierarchy.ts", "Child");
    let runner = slice.symbol("src/core/types.ts", "Runner");
    assert_eq!(
        slice.outgoing(child.id, &[RelationKind::Extends]),
        vec![GraphEndpoint::Symbol(base.id)]
    );
    assert_eq!(
        slice.outgoing(child.id, &[RelationKind::Implements]),
        vec![GraphEndpoint::Symbol(runner.id)]
    );
    assert_eq!(
        slice.outgoing(service.id, &[RelationKind::Implements]),
        vec![GraphEndpoint::Symbol(runner.id)],
        "a cross-file `implements` clause resolves through the alias too"
    );
    let base_run = slice.symbol("src/hierarchy.ts", "Base.run");
    assert_eq!(
        slice.outgoing(
            slice.symbol_at("src/hierarchy.ts", 146).id,
            &[RelationKind::Overrides]
        ),
        vec![GraphEndpoint::Symbol(base_run.id)],
        "`override run()` overrides the base declaration"
    );
    for (rel, at) in [
        ("src/hierarchy.ts", 250),
        ("src/hierarchy.ts", 287),
        ("src/hierarchy.ts", 325),
    ] {
        assert!(
            slice
                .outgoing(slice.symbol_at(rel, at).id, &[RelationKind::Overrides])
                .is_empty(),
            "an unrelated class's `run` overrides nothing: {rel}:{at}"
        );
    }
    assert_eq!(
        slice.count(
            "SELECT COUNT(*) FROM relation r \
             JOIN graph_entity se ON se.id = r.source_entity_id \
             JOIN symbol s ON s.id = se.symbol_id \
             JOIN resource res ON res.id = s.resource_id \
             WHERE r.kind = 'OVERRIDES' AND res.path_key = 'src/samename.ts'"
        ),
        0,
        "three unrelated same-name methods produce no OVERRIDES"
    );

    // ---- The dependency boundary ----------------------------------
    let external_targets =
        slice.outgoing(slice.symbol("src/ext.ts", "ext").id, &[RelationKind::Calls]);
    assert!(
        external_targets
            .iter()
            .any(|target| matches!(target, GraphEndpoint::External(_))),
        "`Buffer.from` is an external identity: {external_targets:?}"
    );
    assert!(
        slice
            .resources()
            .iter()
            .all(|resource| !resource.path_key.contains("node_modules")),
        "no dependency file became a Resource"
    );
    assert_eq!(
        slice.count(
            "SELECT COUNT(*) FROM external_entity WHERE package_identity LIKE '%/%' \
             AND package_identity NOT LIKE '@%'"
        ),
        0,
        "a scoped package is one name, not a path"
    );

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

    // ---- A repeated refresh is idempotent -------------------------
    let before = slice.count("SELECT COUNT(*) FROM relation");
    let again = refresh_everything(&slice, &queries, true);
    assert_eq!(
        slice.count("SELECT COUNT(*) FROM relation"),
        before,
        "asking the same questions again changes nothing"
    );
    assert_eq!(again, evidence, "and produces the same evidence");

    // ---- The Agent-facing surfaces --------------------------------
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
            == GraphEndpoint::Symbol(slice.symbol_at("src/hierarchy.ts", 146).id)),
        "a signature change on Base.run reaches the overriding declaration"
    );
    let interface_impact = ImpactTraversal::open(&slice.db_path)
        .expect("index.db")
        .run(
            ImpactIntent::BaseInterfaceChange,
            &GraphEndpoint::Symbol(runner.id),
            &Budget::default(),
        )
        .expect("impact");
    assert!(
        interface_impact
            .nodes
            .iter()
            .any(|node| node.endpoint == GraphEndpoint::Symbol(service.id)),
        "and an interface change reaches the class that implements it"
    );

    let related = RelatedTests::open(&slice.db_path)
        .expect("index.db")
        .for_target(
            &GraphEndpoint::Symbol(slice.symbol("src/consumer.ts", "consume").id),
            ImpactIntent::PublicSignatureChange,
            &Budget::default(),
        )
        .expect("related tests");
    assert!(
        related
            .candidates
            .iter()
            .any(|candidate| candidate.path_rel.contains("service.test.ts")),
        "the enriched graph is what connects the test to the code it exercises: {:?}",
        related
            .candidates
            .iter()
            .map(|candidate| &candidate.path_rel)
            .collect::<Vec<_>>()
    );

    // Prepared inspection: current source, not a line number to go and
    // read. A hard gate -- a location-only answer where the preparer
    // could have sliced source is an acceptance failure.
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
    let prepared_bytes: usize = prepared.ranges.iter().map(|range| range.source.len()).sum();

    // ---- Freshness, on the real backend ---------------------------
    let index = SemanticIndex::open(&slice.db_path).expect("index.db");
    let consumer_owner = slice.owner("src/consumer.ts");
    assert_eq!(
        index.status(&consumer_owner).expect("status").state,
        SemanticState::Current
    );
    let touched = index
        .invalidate_resource(slice.resource("src/core/service.ts").id)
        .expect("invalidate");
    assert!(
        touched.contains(&consumer_owner),
        "a dependency moving stops the dependent being current"
    );
    assert_ne!(
        index.status(&consumer_owner).expect("status").state,
        SemanticState::Current
    );
    assert!(
        index
            .status(&consumer_owner)
            .expect("status")
            .has_last_valid(),
        "the last valid publication is kept, never replaced with an empty success"
    );

    // ---- The matrix has to match what just happened ---------------
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
        (SemanticCapability::Inheritance, Support::Supported),
        (SemanticCapability::Implements, Support::Supported),
        (SemanticCapability::OverloadResolution, Support::Supported),
        (SemanticCapability::TypeResolution, Support::Partial),
        (SemanticCapability::Overrides, Support::Partial),
        (SemanticCapability::StaticDispatchTarget, Support::Partial),
        (SemanticCapability::ImplementationTarget, Support::Partial),
    ] {
        assert_eq!(declared.support(capability), expected, "{capability:?}");
    }
    // The structural capabilities are I2/I3's, and this backend does
    // not claim them.
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
    // JavaScript is measured, never inferred from TypeScript.
    let javascript = javascript_capability_report(&slice.context);
    for (capability, expected) in [
        (SemanticCapability::ImportBinding, Support::Supported),
        (SemanticCapability::References, Support::Supported),
        (SemanticCapability::CallsIntraFile, Support::Supported),
        (SemanticCapability::CallsCrossFile, Support::Supported),
        (SemanticCapability::Implements, Support::Unsupported),
        (SemanticCapability::OverloadResolution, Support::Unsupported),
        (SemanticCapability::TypeResolution, Support::Unsupported),
        (SemanticCapability::Inheritance, Support::Unsupported),
    ] {
        assert_eq!(
            javascript.support(capability),
            expected,
            "js {capability:?}"
        );
    }
    // The other half of the same contract: what JavaScript declares
    // UNSUPPORTED produced nothing.
    assert_eq!(
        slice.count(
            "SELECT COUNT(*) FROM relation r \
             JOIN graph_entity se ON se.id = r.source_entity_id \
             JOIN symbol s ON s.id = se.symbol_id \
             JOIN resource res ON res.id = s.resource_id \
             WHERE r.kind IN ('EXTENDS', 'IMPLEMENTS', 'OVERRIDES', 'USES_TYPE') \
               AND res.language = 'JAVASCRIPT'"
        ),
        0,
        "a JavaScript type or heritage edge would be a claim with no anchor"
    );

    println!("prepared_source_bytes={prepared_bytes}");
    println!("prepared_ranges={}", prepared.ranges.len());
    println!("semantic_evidence_total={evidence}");
    census(&slice);
}

// ---------------------------------------------------------------------
// Lifecycle, on the real backend
// ---------------------------------------------------------------------

/// The regression the original task named: a native LSP that keeps
/// stale module state after a file is created.
///
/// Measured three ways against 7.0.2 -- an existing module gaining an
/// export, a brand-new module, and a deletion -- with the strict
/// ordering the tier relies on and *no* settle delay anywhere:
///
/// ```text
/// filesystem change → structural update → watched-file notification → request
/// ```
///
/// There is no sleep and no polling in this test, and there must never
/// be one: a settle delay would turn a correctness guarantee into a
/// timing coincidence that happens to hold on a local APFS volume.
#[test]
#[ignore = "needs the pinned typescript install; see the module docs"]
#[allow(clippy::too_many_lines)]
fn the_backend_sees_the_filesystem_as_brainprint_left_it() {
    let Ok(install) = TypeScriptInstall::locate(&install_root()) else {
        println!("skipped: no pinned install");
        return;
    };
    let slice = Slice::open("lifecycle", 32, &install);
    let host = slice
        .launcher
        .start(&slice.binding())
        .expect("server starts");
    let queries = HostQueries {
        host: &host,
        cancel: CancelToken::new(),
    };
    let encoding = slice.launcher.negotiated_encoding().expect("encoding");
    refresh_one(&slice, &queries, encoding, "src/consumer.ts");

    // ---- An existing module gains an export ----------------------
    let outcome = apply(
        &slice,
        &queries,
        encoding,
        "workspace-rev-2",
        &[
            ("src/model.ts", lifecycle::ChangeKind::Changed),
            ("src/consumer.ts", lifecycle::ChangeKind::Changed),
        ],
        || {
            write(
                &slice,
                "src/model.ts",
                "export class Model {\n    describe(): string {\n        return \"model\";\n    }\n}\n\nexport const added = 1;\n",
            );
            write(
                &slice,
                "src/consumer.ts",
                "import { added } from \"./model.js\";\n\nexport function useAdded(): number {\n    return added;\n}\n",
            );
        },
    );
    assert!(
        outcome.report.iter().any(|line| line.contains("RESOLVED")),
        "a new export is visible on the first request after the notification, \
         with no settle delay anywhere: {:?}",
        outcome.report
    );

    // ---- A brand-new module --------------------------------------
    apply(
        &slice,
        &queries,
        encoding,
        "workspace-rev-3",
        &[
            ("src/fresh.ts", lifecycle::ChangeKind::Added),
            ("src/consumer.ts", lifecycle::ChangeKind::Changed),
        ],
        || {
            write(
                &slice,
                "src/fresh.ts",
                "export function fresh(): number {\n    return 1;\n}\n",
            );
            write(
                &slice,
                "src/consumer.ts",
                "import { fresh } from \"./fresh.js\";\n\nexport function useFresh(): number {\n    return fresh();\n}\n",
            );
        },
    );
    assert!(
        slice
            .outgoing(
                slice.symbol("src/consumer.ts", "useFresh").id,
                &[RelationKind::Calls]
            )
            .contains(&GraphEndpoint::Symbol(
                slice.symbol("src/fresh.ts", "fresh").id
            )),
        "a module created after the server started resolves through it"
    );

    // ---- A deletion ----------------------------------------------
    let outcome = apply(
        &slice,
        &queries,
        encoding,
        "workspace-rev-4",
        &[
            ("src/fresh.ts", lifecycle::ChangeKind::Deleted),
            ("src/consumer.ts", lifecycle::ChangeKind::Changed),
        ],
        || {
            fs::remove_file(slice.workspace.join("src/fresh.ts")).expect("remove");
            write(
                &slice,
                "src/consumer.ts",
                "import { fresh } from \"./fresh.js\";\n\nexport function useFresh(): number {\n    return fresh();\n}\n",
            );
        },
    );
    assert!(
        !outcome
            .report
            .iter()
            .any(|line| line.contains("RESOLVED Symbol")),
        "the deleted module's proof is gone, not served stale: {:?}",
        outcome.report
    );

    // ---- A path alias retarget -----------------------------------
    // No source moves at all; only the configuration does. The owners
    // have to stop being current anyway.
    let index = SemanticIndex::open(&slice.db_path).expect("index.db");
    let before = lifecycle::discover_config(index.connection(), &slice.workspace, "")
        .expect("config")
        .basis()
        .fingerprint();
    drop(index);
    write(
        &slice,
        "tsconfig.json",
        "{ \"extends\": \"./tsconfig.base.json\",\n  \"compilerOptions\": { \"baseUrl\": \".\", \"paths\": { \"@core/*\": [\"src/*\"] } },\n  \"include\": [\"src\", \"js\", \"tests\"] }\n",
    );
    slice.rescan("workspace-rev-5");
    let index = SemanticIndex::open(&slice.db_path).expect("index.db");
    let config =
        lifecycle::discover_config(index.connection(), &slice.workspace, "").expect("config");
    assert_ne!(
        config.basis().fingerprint(),
        before,
        "retargeting `@core/*` is a different configuration"
    );
    let change = lifecycle::ResourceChange::new(
        slice.resource("tsconfig.json").id,
        lifecycle::ChangeKind::Changed,
        "tsconfig.json",
    )
    .with_language(None);
    let plan = lifecycle::plan_changes(&index, &slice.context, &[change], &config).expect("plan");
    assert!(plan.config_moved);
    assert!(
        !plan.affected.is_empty(),
        "a config change reaches every owner the project published"
    );
}

/// Write a file into the Workspace.
fn write(slice: &Slice, rel: &str, contents: &str) {
    fs::write(slice.workspace.join(rel), contents).expect("write");
}

/// One whole lifecycle turn, in the order the tier requires.
///
/// ```text
/// plan → withdraw → structural replacement → watched-file batch → refresh
/// ```
///
/// Withdrawal before replacement is not tidiness: `semantic_evidence`
/// has no cascade, so a contribution pointing at relations the
/// replacement is about to remove has to be withdrawn first. Skipping
/// it fails the structural publication outright, which is how this test
/// found the contract the first time it was written the other way
/// round.
///
/// And there is no sleep in here. A settle delay would turn a
/// correctness guarantee into a timing coincidence.
fn apply(
    slice: &Slice,
    queries: &dyn TypeScriptQueries,
    encoding: brainprint_engine::typescript_semantic::protocol::PositionEncodingChoice,
    revision: &str,
    changes: &[(&str, lifecycle::ChangeKind)],
    mutate: impl FnOnce(),
) -> brainprint_engine::typescript_semantic::RefreshOutcome {
    let planned: Vec<lifecycle::ResourceChange> = changes
        .iter()
        .filter_map(|(rel, kind)| {
            slice
                .resources()
                .into_iter()
                .find(|resource| resource.path_key == *rel)
                .map(|resource| lifecycle::ResourceChange::new(resource.id, *kind, *rel))
        })
        .collect();

    {
        let index = SemanticIndex::open(&slice.db_path).expect("index.db");
        let config =
            lifecycle::discover_config(index.connection(), &slice.workspace, "").expect("config");
        let plan =
            lifecycle::plan_changes(&index, &slice.context, &planned, &config).expect("plan");
        lifecycle::withdraw_affected(
            &index,
            &plan.affected,
            brainprint_engine::semantic_index::SOURCE_MOVED_CODE,
        )
        .expect("withdraw before the replacement that re-resolves these sites");
    }

    mutate();
    slice.rescan(revision);

    let notified: Vec<lifecycle::ResourceChange> = changes
        .iter()
        .map(|(rel, kind)| {
            let carrier = slice
                .resources()
                .into_iter()
                .find(|resource| resource.path_key == *rel)
                .unwrap_or_else(|| slice.resource("tsconfig.json"));
            lifecycle::ResourceChange::new(carrier.id, *kind, *rel)
        })
        .collect();
    lifecycle::synchronize(queries, &slice.workspace, &notified).expect("notified");

    refresh_one(slice, queries, encoding, "src/consumer.ts")
}

/// Several callers of one AnalysisContext share one server process.
#[test]
#[ignore = "needs the pinned typescript install; see the module docs"]
fn one_analysis_context_runs_one_native_language_server() {
    let Ok(install) = TypeScriptInstall::locate(&install_root()) else {
        println!("skipped: no pinned install");
        return;
    };
    let slice = Slice::open("shared-runtime", 33, &install);
    let supervisor = SemanticRuntimeSupervisor::new(RuntimePolicy::default())
        .with_backend(Arc::clone(&slice.launcher) as Arc<dyn SemanticBackendLauncher>);
    let first = supervisor.acquire(&slice.binding()).expect("first caller");
    let second = supervisor.acquire(&slice.binding()).expect("second caller");
    let third = supervisor.acquire(&slice.binding()).expect("third caller");
    assert_eq!(
        supervisor.live_runtime_count(),
        1,
        "three callers, one `tsc --lsp --stdio`"
    );
    drop((first, second, third));
    supervisor.shutdown();
}

/// Refresh one owner and return what it produced.
fn refresh_one(
    slice: &Slice,
    queries: &dyn TypeScriptQueries,
    encoding: brainprint_engine::typescript_semantic::protocol::PositionEncodingChoice,
    rel: &str,
) -> brainprint_engine::typescript_semantic::RefreshOutcome {
    let index = SemanticIndex::open(&slice.db_path).expect("index.db");
    let config = lifecycle::discover_config(index.connection(), &slice.workspace, "")
        .expect("config")
        .basis();
    let capabilities = capability_report(&slice.context);
    refresh_resource(
        &index,
        queries,
        &RefreshRequest {
            context: &slice.context,
            workspace_root: &slice.workspace,
            owner: slice.resource(rel).id,
            config: &config,
            capabilities: &capabilities,
            encoding,
        },
    )
    .unwrap_or_else(|error| panic!("{rel}: {error}"))
}

/// Everything the capability matrices are read off.
#[allow(clippy::too_many_lines)]
fn census(slice: &Slice) {
    println!("\n=== census ===");
    println!(
        "semantic relations: {}",
        slice.count(
            "SELECT COUNT(*) FROM relation r JOIN semantic_evidence se ON se.relation_id = r.id"
        )
    );
    println!(
        "conflicts: {}",
        slice.count("SELECT COUNT(*) FROM semantic_conflict")
    );
    println!(
        "open gaps: {}",
        slice.count("SELECT COUNT(*) FROM unresolved_reference")
    );
    println!(
        "external entities: {}",
        slice.count("SELECT COUNT(*) FROM external_entity")
    );
}
