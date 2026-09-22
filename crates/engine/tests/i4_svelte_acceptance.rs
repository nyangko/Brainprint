//! Svelte container acceptance, against the real pinned language tools
//! (#19 task 11).
//!
//! What this proves that the scripted tests cannot: that the answers the
//! adapter is built around are the answers `svelte-language-server`
//! 0.18.4 actually gives, over a real component tree, driven through
//! Brainprint's own launcher, host and protocol code.
//!
//! Every fact travels the whole tier and is read back through the
//! ordinary APIs:
//!
//! ```text
//! .svelte → LSP → adapter → evidence → publication → merge → query API
//! ```
//!
//! ```sh
//! cd scripts/svelte_semantic_spike && npm install && cd -
//! cargo test -p brainprint-engine --test i4_svelte_acceptance -- --ignored --nocapture
//! ```

use std::{
    env, fs,
    path::{Path, PathBuf},
    process,
    sync::Arc,
};

use brainprint_core::{ResourceId, WorkspaceId};
use brainprint_engine::{
    config::WorkspaceConfig,
    graph::{GraphEndpoint, RelationKind},
    impact::{Budget, ImpactIntent, ImpactTraversal},
    prepare::InspectPreparer,
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
    structural::StructuralState,
    svelte_semantic::{
        RefreshRequest, SvelteHost, SvelteInstall, SvelteLauncher, SvelteQueries,
        capability_report, lifecycle,
        protocol::{SvelteRequest, SvelteResponse},
        refresh_resource, toolchain_identity,
    },
    symbol::{Symbol, SymbolStore},
};
use rusqlite::Connection;

// ---------------------------------------------------------------------
// Fixture plumbing
// ---------------------------------------------------------------------

fn install_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/svelte_semantic_spike")
}

fn fixture_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/workspaces/svelte-semantic-spike")
}

/// The Node runtime. Supplied, never searched for by the engine itself.
fn node() -> String {
    env::var("BRAINPRINT_NODE").unwrap_or_else(|_| "node".to_owned())
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
struct HostQueries<'a> {
    host: &'a SvelteHost,
    cancel: CancelToken,
}

impl SvelteQueries for HostQueries<'_> {
    fn call(&self, request: &SvelteRequest) -> Result<SvelteResponse, RequestFailure> {
        self.host
            .call(request, &self.cancel)
            .map_err(RequestFailure::Backend)
    }
}

/// One indexed Workspace with one Svelte language server behind it.
struct Slice {
    workspace: PathBuf,
    db_path: PathBuf,
    base: PathBuf,
    context: AnalysisContext,
    launcher: Arc<SvelteLauncher>,
}

impl Slice {
    fn open(label: &str, workspace_uid: u8, install: &SvelteInstall) -> Self {
        let base = env::temp_dir().join(format!("brainprint-i4svelte-{label}-{}", process::id()));
        let _ = fs::remove_dir_all(&base);
        let workspace = base.join("workspace");
        copy_tree(&fixture_source(), &workspace);
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
            lifecycle::environment_identity(index.connection(), &workspace, "", install)
                .expect("environment")
        };
        let context = AnalysisContext {
            workspace: WorkspaceId::from_bytes([workspace_uid; 16]),
            // A distinct backend kind and language, which is what lets
            // this run its own TypeScript beside the TS/JS backend's.
            backend: SemanticBackendKind::Svelte,
            language: ResourceLanguage::Svelte,
            project_root: ProjectRootIdentity::Key(format!("svelte-spike-{label}")),
            toolchain: toolchain_identity(install, &environment),
        };
        Self {
            workspace,
            db_path,
            base,
            context,
            launcher: Arc::new(SvelteLauncher::new(install.clone(), node())),
        }
    }

    fn binding(&self) -> AnalysisContextBinding {
        AnalysisContextBinding {
            context: self.context.clone(),
            project_root_rel: self.workspace.to_string_lossy().into_owned(),
            config_file_rel: None,
        }
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

    fn components(&self) -> Vec<String> {
        let mut found: Vec<String> = self
            .resources()
            .into_iter()
            .filter(|resource| resource.language == Some(ResourceLanguage::Svelte))
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

    fn text(&self, rel: &str) -> String {
        fs::read_to_string(self.workspace.join(rel)).expect("source")
    }
}

impl Drop for Slice {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

/// Every component, announced and refreshed once.
fn refresh_everything(slice: &Slice, queries: &dyn SvelteQueries) -> usize {
    let index = SemanticIndex::open(&slice.db_path).expect("index.db");
    // The barrier. A component the server has never been told about is
    // an error rather than an empty answer, so this is what makes the
    // first question answerable at all -- and there is no sleep in it.
    lifecycle::announce_components(&index, queries, &slice.workspace).expect("announce");

    let config = lifecycle::discover_config(index.connection(), "").expect("config");
    let basis = config.basis();
    let capabilities = capability_report(&slice.context);
    let mut evidence = 0;
    for rel in slice.components() {
        let outcome = refresh_resource(
            &index,
            queries,
            &RefreshRequest {
                context: &slice.context,
                workspace_root: &slice.workspace,
                owner: slice.resource(&rel).id,
                config: &basis,
                capabilities: &capabilities,
            },
        )
        .unwrap_or_else(|error| panic!("{rel}: {error}"));
        evidence += outcome.evidence_count;
        println!("--- {rel}: {} facts", outcome.evidence_count);
        for line in &outcome.report {
            println!("      {line}");
        }
        for (failure, uri) in &outcome.mapping_failures {
            println!("      MAPPING REFUSED {failure:?} {uri}");
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
#[ignore = "needs the pinned svelte language tools; see the module docs"]
#[allow(clippy::too_many_lines)]
fn the_svelte_container_slice_holds_against_the_real_backend() {
    let Ok(install) = SvelteInstall::locate(&install_root()) else {
        println!(
            "skipped: no pinned install under {}",
            install_root().display()
        );
        return;
    };
    println!(
        "svelte-language-server {} · svelte2tsx {:?} · svelte {:?} · typescript {:?}",
        install.server_version,
        install.companion("svelte2tsx"),
        install.companion("svelte"),
        install.companion("typescript"),
    );

    let slice = Slice::open("container", 41, &install);
    println!("launch: {}", slice.launcher.command_line());
    let host = slice
        .launcher
        .start(&slice.binding())
        .expect("server starts");
    let queries = HostQueries {
        host: &host,
        cancel: CancelToken::new(),
    };

    let evidence = refresh_everything(&slice, &queries);
    assert!(evidence > 0);

    // ---- The canonical Resource is the component -------------------
    assert!(
        slice
            .resources()
            .iter()
            .all(|resource| !resource.path_key.ends_with(".svelte.ts")
                && !resource.path_key.ends_with(".svelte.tsx")
                && !resource.path_key.contains("__sveltets")
                && !resource.path_key.contains("node_modules")),
        "no generated file and no dependency became a Resource"
    );
    assert_eq!(
        brainprint_engine::structural::read(
            SemanticIndex::open(&slice.db_path)
                .expect("index.db")
                .connection(),
            parent_resource_id(&slice)
        )
        .expect("structure")
        .expect("recorded")
        .state,
        StructuralState::ContainerOnly,
        "a component is still a container: its `<style>` is not indexed, \
         and zero findings there is coverage rather than a fact"
    );

    // ---- Template identifiers reach their script declarations ------
    let parent = slice.resource("src/Parent.svelte");
    let _ = &parent;
    let increment = slice.symbol("src/Parent.svelte", "increment");
    let count = slice.symbol("src/Parent.svelte", "count");
    let from_template = slice.resource_outgoing(parent.id, &[RelationKind::References]);
    assert!(
        from_template.contains(&GraphEndpoint::Symbol(increment.id)),
        "`onclick={{increment}}` reaches the script function: {from_template:?}"
    );
    assert!(
        from_template.contains(&GraphEndpoint::Symbol(count.id)),
        "and `{{count}}` reaches the script state: {from_template:?}"
    );

    // Every one of those anchors on a span of the *component*, and the
    // text at that span is what the markup writes.
    let parent_source = slice.text("src/Parent.svelte");
    let mut statement = Connection::open(&slice.db_path).expect("index.db");
    let transaction = statement.transaction().expect("read");
    {
        let mut rows = transaction
            .prepare(
                "SELECT occurrence.start_byte, occurrence.end_byte FROM semantic_evidence \
                 JOIN occurrence ON occurrence.id = semantic_evidence.occurrence_id \
                 JOIN resource ON resource.id = occurrence.resource_id \
                 WHERE resource.path_key = 'src/Parent.svelte' \
                   AND occurrence.kind = 'REFERENCE_SITE'",
            )
            .expect("prepare");
        let spans: Vec<(usize, usize)> = rows
            .query_map([], |row| {
                Ok((
                    usize::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
                    usize::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                ))
            })
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("spans");
        assert!(!spans.is_empty());
        for (start, end) in spans {
            assert!(
                end <= parent_source.len(),
                "a span outside the component would be a generated offset"
            );
            let written = &parent_source[start..end];
            assert!(
                written
                    .chars()
                    .all(|character| character.is_alphanumeric() || character == '_'),
                "{written:?} is not an identifier the component writes"
            );
        }
    }
    drop(transaction);

    // ---- Component usage reaches the component ---------------------
    let child = slice.resource("src/lib/Child.svelte");
    let other = slice.resource("src/lib/OtherChild.svelte");
    assert!(
        from_template.contains(&GraphEndpoint::Resource(child.id)),
        "`<Child …/>` reaches the component's own Resource, not a \
         generated class: {from_template:?}"
    );
    assert!(from_template.contains(&GraphEndpoint::Resource(other.id)));

    // The same-name trap: `SameName.svelte` writes `<Child>` too, and
    // imports it from `OtherChild.svelte`.
    let same_name = slice.resource("src/SameName.svelte");
    let trapped = slice.resource_outgoing(same_name.id, &[RelationKind::References]);
    assert!(
        trapped.contains(&GraphEndpoint::Resource(other.id)),
        "the trap resolves to what *this* file imported: {trapped:?}"
    );
    assert!(
        !trapped.contains(&GraphEndpoint::Resource(child.id)),
        "and never to the same-named component next door: {trapped:?}"
    );

    // ---- Script semantics, TypeScript and plain JavaScript ---------
    let model = slice.symbol("src/lib/model.ts", "Model");
    let script_targets = slice.resource_outgoing(
        parent.id,
        &[
            RelationKind::Imports,
            RelationKind::References,
            RelationKind::UsesType,
        ],
    );
    assert!(
        script_targets.contains(&GraphEndpoint::Symbol(model.id)),
        "`let model: Model` inside `<script lang=\"ts\">` reaches the \
         interface in the `.ts` beside it: {script_targets:?}"
    );
    assert!(
        script_targets.contains(&GraphEndpoint::Resource(child.id)),
        "and `import Child from './lib/Child.svelte'` reaches the component"
    );

    let javascript = slice.resource("src/JavaScriptParent.svelte");
    let from_js = slice.resource_outgoing(javascript.id, &[RelationKind::References]);
    assert!(
        from_js.contains(&GraphEndpoint::Symbol(
            slice.symbol("src/JavaScriptParent.svelte", "bump").id
        )),
        "a plain `<script>` component resolves its own template too: {from_js:?}"
    );

    // ---- Nothing generated, ever -----------------------------------
    assert_eq!(
        slice.count(
            "SELECT COUNT(*) FROM external_entity WHERE package_identity LIKE '%sveltets%' \
             OR COALESCE(module_path, '') LIKE '%sveltets%'"
        ),
        0,
        "no generated helper became an identity"
    );

    // ---- One logical relation, not one per tier --------------------
    assert_eq!(
        slice.count(
            "SELECT COUNT(*) FROM (SELECT kind, source_entity_id, target_entity_id, \
             COUNT(*) AS n FROM relation \
             GROUP BY kind, source_entity_id, target_entity_id HAVING n > 1)"
        ),
        0
    );

    // ---- A repeated refresh is idempotent --------------------------
    let before = slice.count("SELECT COUNT(*) FROM relation");
    let again = refresh_everything(&slice, &queries);
    assert_eq!(slice.count("SELECT COUNT(*) FROM relation"), before);
    assert_eq!(again, evidence);

    // ---- The Agent-facing surfaces ---------------------------------
    // A template use is a written mention of a name, which is exactly
    // what `Rename` traverses.
    let impact = ImpactTraversal::open(&slice.db_path)
        .expect("index.db")
        .run(
            ImpactIntent::Rename,
            &GraphEndpoint::Symbol(increment.id),
            &Budget::default(),
        )
        .expect("impact");
    assert!(
        impact
            .nodes
            .iter()
            .any(|node| node.endpoint == GraphEndpoint::Resource(parent.id)),
        "renaming a script declaration reaches the component whose \
         template writes it: {:?}",
        impact
            .nodes
            .iter()
            .map(|node| &node.endpoint)
            .collect::<Vec<_>>()
    );
    // And the same traversal from a *component* reaches the components
    // that use it, which is the component-usage relation being consumed
    // by an ordinary query surface.
    let used_by = ImpactTraversal::open(&slice.db_path)
        .expect("index.db")
        .run(
            ImpactIntent::Rename,
            &GraphEndpoint::Resource(child.id),
            &Budget::default(),
        )
        .expect("impact");
    assert!(
        used_by
            .nodes
            .iter()
            .any(|node| node.endpoint == GraphEndpoint::Resource(parent.id)),
        "a component change reaches the components that render it"
    );

    // Prepared inspection: current *component* source, never generated.
    let prepared = InspectPreparer::open(&slice.db_path, &slice.workspace)
        .expect("preparer")
        .prepare(
            &GraphEndpoint::Symbol(increment.id),
            Direction::Incoming,
            &[RelationKind::References],
        )
        .expect("prepared");
    assert!(prepared.confirmed_count() > 0);
    assert!(
        prepared.source_complete(),
        "every prepared range carries verified current source"
    );
    for range in &prepared.ranges {
        assert!(
            !range.source.contains("__sveltets"),
            "generated source must never be prepared: {:?}",
            range.source
        );
    }
    assert!(
        prepared
            .ranges
            .iter()
            .any(|range| range.source.contains("increment")),
        "the Agent gets the source, not a path and a line number"
    );
    let prepared_bytes: usize = prepared.ranges.iter().map(|range| range.source.len()).sum();

    // ---- Freshness --------------------------------------------------
    let index = SemanticIndex::open(&slice.db_path).expect("index.db");
    let owner = slice.owner("src/Parent.svelte");
    assert_eq!(
        index.status(&owner).expect("status").state,
        SemanticState::Current
    );
    let touched = index
        .invalidate_resource(slice.resource("src/lib/Child.svelte").id)
        .expect("invalidate");
    assert!(
        touched.contains(&owner),
        "a component whose proof read another component goes stale with it"
    );
    assert!(index.status(&owner).expect("status").has_last_valid());

    // ---- The matrix has to match what just happened ----------------
    let declared = capability_report(&slice.context);
    for (capability, expected) in [
        (
            SemanticCapability::EmbeddedRegionMapping,
            Support::Supported,
        ),
        (
            SemanticCapability::OriginalSourceMapping,
            Support::Supported,
        ),
        (SemanticCapability::SymbolDefinition, Support::Supported),
        (SemanticCapability::ImportBinding, Support::Supported),
        (SemanticCapability::References, Support::Supported),
        (SemanticCapability::TypeResolution, Support::Supported),
        (SemanticCapability::Inheritance, Support::Unsupported),
        (SemanticCapability::Implements, Support::Unsupported),
        (SemanticCapability::Overrides, Support::Unsupported),
    ] {
        assert_eq!(declared.support(capability), expected, "{capability:?}");
    }
    assert_eq!(
        slice.count(
            "SELECT COUNT(*) FROM relation WHERE kind IN ('EXTENDS','IMPLEMENTS','OVERRIDES')"
        ),
        0,
        "a declared-UNSUPPORTED capability produced nothing"
    );

    println!("prepared_source_bytes={prepared_bytes}");
    println!("semantic_evidence_total={evidence}");
    census(&slice);
}

/// The lifecycle, on the real backend.
#[test]
#[ignore = "needs the pinned svelte language tools; see the module docs"]
#[allow(clippy::too_many_lines)]
fn a_component_edit_moves_every_span_and_no_stale_proof_survives() {
    let Ok(install) = SvelteInstall::locate(&install_root()) else {
        println!("skipped: no pinned install");
        return;
    };
    let slice = Slice::open("lifecycle", 42, &install);
    let host = slice
        .launcher
        .start(&slice.binding())
        .expect("server starts");
    let queries = HostQueries {
        host: &host,
        cancel: CancelToken::new(),
    };
    refresh_everything(&slice, &queries);

    let increment_before = slice.symbol("src/Parent.svelte", "increment");
    let parent = slice.resource("src/Parent.svelte");
    assert!(
        slice
            .resource_outgoing(parent.id, &[RelationKind::References])
            .contains(&GraphEndpoint::Symbol(increment_before.id))
    );

    // Insert two lines into the script. Every template offset below
    // moves, and so does every generated offset the server works with.
    // A mapping reused from before the edit would now be wrong.
    let edited = slice.text("src/Parent.svelte").replace(
        "    let count = $state(0);",
        "    // inserted line one\n    // inserted line two\n    let count = $state(0);",
    );
    apply(
        &slice,
        &queries,
        "workspace-rev-2",
        &[("src/Parent.svelte", lifecycle::ChangeKind::Changed)],
        || {
            fs::write(slice.workspace.join("src/Parent.svelte"), &edited).expect("write");
        },
    );

    let increment_after = slice.symbol("src/Parent.svelte", "increment");
    assert_ne!(
        increment_after.span.start_byte, increment_before.span.start_byte,
        "the declaration moved"
    );
    let after = slice.resource_outgoing(
        slice.resource("src/Parent.svelte").id,
        &[RelationKind::References],
    );
    assert!(
        after.contains(&GraphEndpoint::Symbol(increment_after.id)),
        "and the template reference follows it to the new position: {after:?}"
    );
    // Every anchor is inside the *current* component.
    let current = slice.text("src/Parent.svelte");
    let longest: i64 = slice.count(
        "SELECT COALESCE(MAX(occurrence.end_byte), 0) FROM semantic_evidence \
         JOIN occurrence ON occurrence.id = semantic_evidence.occurrence_id \
         JOIN resource ON resource.id = occurrence.resource_id \
         WHERE resource.path_key = 'src/Parent.svelte'",
    );
    assert!(
        usize::try_from(longest).expect("fits") <= current.len(),
        "no anchor survives past the end of the current source"
    );

    // Retarget the import: the same `<Child>` tag, a different component.
    let retargeted = current.replace(
        "import Child from './lib/Child.svelte';",
        "import Child from './lib/OtherChild.svelte';",
    );
    apply(
        &slice,
        &queries,
        "workspace-rev-3",
        &[("src/Parent.svelte", lifecycle::ChangeKind::Changed)],
        || {
            fs::write(slice.workspace.join("src/Parent.svelte"), &retargeted).expect("write");
        },
    );
    let retargeted_edges = slice.resource_outgoing(
        slice.resource("src/Parent.svelte").id,
        &[RelationKind::References],
    );
    assert!(
        !retargeted_edges.contains(&GraphEndpoint::Resource(
            slice.resource("src/lib/Child.svelte").id
        )),
        "the old component edge is gone, not left behind: {retargeted_edges:?}"
    );

    // A new component, created after the server started.
    apply(
        &slice,
        &queries,
        "workspace-rev-4",
        &[
            ("src/lib/Fresh.svelte", lifecycle::ChangeKind::Added),
            ("src/Parent.svelte", lifecycle::ChangeKind::Changed),
        ],
        || {
            fs::write(
                slice.workspace.join("src/lib/Fresh.svelte"),
                "<script lang=\"ts\">\n    let { tag }: { tag: string } = $props();\n</script>\n\n<b>{tag}</b>\n",
            )
            .expect("write");
            let with_fresh = fs::read_to_string(slice.workspace.join("src/Parent.svelte"))
                .expect("read")
                .replace(
                    "import Child from './lib/OtherChild.svelte';",
                    "import Child from './lib/OtherChild.svelte';\n    import Fresh from './lib/Fresh.svelte';",
                )
                .replace("<OtherChild label={summary} />", "<OtherChild label={summary} />\n<Fresh tag={summary} />");
            fs::write(slice.workspace.join("src/Parent.svelte"), with_fresh).expect("write");
        },
    );
    assert!(
        slice
            .resource_outgoing(
                slice.resource("src/Parent.svelte").id,
                &[RelationKind::References]
            )
            .contains(&GraphEndpoint::Resource(
                slice.resource("src/lib/Fresh.svelte").id
            )),
        "a component created after the server started resolves through it"
    );
}

/// Several callers of one AnalysisContext share one server process.
#[test]
#[ignore = "needs the pinned svelte language tools; see the module docs"]
fn one_analysis_context_runs_one_svelte_language_server() {
    let Ok(install) = SvelteInstall::locate(&install_root()) else {
        println!("skipped: no pinned install");
        return;
    };
    let slice = Slice::open("shared-runtime", 43, &install);
    let supervisor = SemanticRuntimeSupervisor::new(RuntimePolicy::default())
        .with_backend(Arc::clone(&slice.launcher) as Arc<dyn SemanticBackendLauncher>);
    let first = supervisor.acquire(&slice.binding()).expect("first caller");
    let second = supervisor.acquire(&slice.binding()).expect("second caller");
    assert_eq!(
        supervisor.live_runtime_count(),
        1,
        "two callers, one language server -- and never one per component"
    );
    drop((first, second));
    supervisor.shutdown();
}

/// One whole lifecycle turn, in the order the tier requires.
fn apply(
    slice: &Slice,
    queries: &dyn SvelteQueries,
    revision: &str,
    changes: &[(&str, lifecycle::ChangeKind)],
    mutate: impl FnOnce(),
) {
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
        let config = lifecycle::discover_config(index.connection(), "").expect("config");
        let plan =
            lifecycle::plan_changes(&index, &slice.context, &planned, &config).expect("plan");
        // Withdrawal before the structural replacement that re-resolves
        // these sites. `semantic_evidence` has no cascade, so this is a
        // contract rather than an error to catch.
        // What `plan_changes` selected -- asserted, because selective
        // invalidation is the point of having a plan at all.
        assert!(
            plan.affected.contains(&slice.owner("src/Parent.svelte")),
            "the edited component is in the plan"
        );
        // This harness then replays the *whole* baseline scan rather
        // than an incremental structural update, and a whole-workspace
        // replacement re-resolves every component. So every published
        // owner is withdrawn here: the ordering contract is about what
        // the replacement touches, and this one touches everything.
        let mut withdrawing = plan.affected.clone();
        withdrawing.extend(
            index
                .owners_of_context(&slice.context.context_key())
                .expect("owners"),
        );
        lifecycle::withdraw_affected(
            &index,
            &withdrawing,
            brainprint_engine::semantic_index::SOURCE_MOVED_CODE,
        )
        .expect("withdraw");
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
                .unwrap_or_else(|| slice.resource("src/Parent.svelte"));
            lifecycle::ResourceChange::new(carrier.id, *kind, *rel)
        })
        .collect();
    // No sleep, no polling: the notification and the requests after it
    // travel the same ordered connection.
    lifecycle::synchronize(queries, &slice.workspace, &notified).expect("notified");

    let index = SemanticIndex::open(&slice.db_path).expect("index.db");
    let config = lifecycle::discover_config(index.connection(), "")
        .expect("config")
        .basis();
    let capabilities = capability_report(&slice.context);
    for rel in slice.components() {
        refresh_resource(
            &index,
            queries,
            &RefreshRequest {
                context: &slice.context,
                workspace_root: &slice.workspace,
                owner: slice.resource(&rel).id,
                config: &config,
                capabilities: &capabilities,
            },
        )
        .unwrap_or_else(|error| panic!("{rel}: {error}"));
    }
}

fn parent_resource_id(slice: &Slice) -> ResourceId {
    slice.resource("src/Parent.svelte").id
}

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
        "component symbols: {}",
        slice.count(
            "SELECT COUNT(*) FROM symbol s JOIN resource r ON r.id = s.resource_id \
             WHERE r.language = 'SVELTE'"
        )
    );
    println!(
        "template use sites: {}",
        slice.count(
            "SELECT COUNT(*) FROM occurrence o JOIN resource r ON r.id = o.resource_id \
             WHERE r.language = 'SVELTE' AND o.kind = 'REFERENCE_SITE'"
        )
    );
    println!(
        "resources: {}",
        slice.count("SELECT COUNT(*) FROM resource")
    );
}
