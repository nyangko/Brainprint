//! Lifecycle tests for the Python semantic tier (#19 task 8).
//!
//! Deterministic throughout: a scripted backend, a real indexed
//! Workspace, and no Node anywhere. The real backend has its own
//! integration test; what these prove is the ordering, the
//! granularity and the degradation, which a fake host demonstrates
//! more reliably than a real one.

use std::{collections::BTreeSet, fs, sync::Arc, time::Duration};

use super::{
    adapter::{BatchPolicy, PythonQueries},
    host::PythonSettings,
    lifecycle::{
        self, BackendReadiness, ChangeKind, ConfigSource, EnvironmentAssurance, EnvironmentFs,
        EnvironmentIdentity, RealFs, ResourceChange, SemanticAvailability,
    },
    protocol::{PythonRequest, WatchedChangeKind},
    tests_support::{Fixture, ScriptedBackend, context},
    *,
};
use crate::{
    config::WorkspaceConfig,
    evidence::list_unresolved_for_resource,
    graph::{GraphEndpoint, GraphStore, RelationKind},
    relations::RelationIndex,
    resolution::Support,
    resource::ResourceLanguage,
    runtime::{
        HostError, RuntimePolicy, RuntimeState, SemanticBackendLauncher, SemanticRuntimeHost,
        SemanticRuntimeSupervisor, StartFailure,
    },
    scan::BaselineScan,
    semantic::{AnalysisContextBinding, SemanticBackendKind},
    semantic_index::{
        BACKEND_UNAVAILABLE_CODE, ENVIRONMENT_CHANGED_CODE, ENVIRONMENT_UNPROVEN_CODE,
        SemanticIndex, SemanticOwner, SemanticState, SemanticStatus,
    },
    symbol::SymbolStore,
};

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

fn owner_of(fixture: &Fixture, rel: &str) -> SemanticOwner {
    SemanticOwner::new(context().context_key(), fixture.resource(rel).id)
}

fn status_of(fixture: &Fixture, rel: &str) -> SemanticStatus {
    SemanticIndex::open(&fixture.db_path())
        .expect("index.db")
        .status(&owner_of(fixture, rel))
        .expect("status")
}

fn state_of(fixture: &Fixture, rel: &str) -> SemanticState {
    status_of(fixture, rel).state
}

fn symbol(fixture: &Fixture, rel: &str, qualified_name: &str) -> brainprint_core::SymbolId {
    SymbolStore::open(&fixture.db_path())
        .expect("index.db")
        .list_for_resource(fixture.resource(rel).id)
        .expect("symbols")
        .into_iter()
        .find(|symbol| symbol.qualified_name == qualified_name)
        .unwrap_or_else(|| panic!("{qualified_name} in {rel}"))
        .id
}

fn overrides_of(fixture: &Fixture, from: brainprint_core::SymbolId) -> Vec<GraphEndpoint> {
    RelationIndex::open(&fixture.db_path())
        .expect("index.db")
        .outgoing(&GraphEndpoint::Symbol(from), &[RelationKind::Overrides])
        .expect("outgoing")
        .confirmed
        .into_iter()
        .map(|relation| relation.target)
        .collect()
}

fn count(fixture: &Fixture, table: &str) -> i64 {
    GraphStore::open(&fixture.db_path())
        .expect("index.db")
        .connection()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count")
}

/// The scripted answers the Python fixture needs, for any file.
fn backend(fixture: &Fixture) -> ScriptedBackend {
    let last = |rel: &str, needle: &str, nth: usize| {
        let range = fixture.range(rel, needle, nth);
        coordinates::Position::new(range.end.line, range.end.character - 1)
    };
    ScriptedBackend::new()
        .with_search_paths(Vec::new())
        // `x.run(1)` -> Base.run, which is what the derived override
        // and the cross-file call both hang on.
        .with_definition(
            &fixture.uri("pkg/impl.py"),
            last("pkg/impl.py", "x.run", 0),
            vec![fixture.location("pkg/base.py", "run", 0)],
        )
        // A re-export chain I3 stops at, so `uses.py` has a
        // contribution of its own to be left alone.
        .with_definition(
            &fixture.uri("pkg/uses.py"),
            last("pkg/uses.py", "Exported", 1),
            vec![fixture.location("pkg/base.py", "Base", 0)],
        )
}

fn refresh(
    fixture: &Fixture,
    queries: &dyn PythonQueries,
    rel: &str,
) -> Result<RefreshOutcome, PythonSemanticError> {
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let context = context();
    let capabilities = capability_report(&context);
    let config = config_basis(&PythonSettings::default(), None);
    refresh_resource(
        &index,
        queries,
        &RefreshRequest {
            context: &context,
            workspace_root: &fixture.root,
            owner: fixture.resource(rel).id,
            config: &config,
            capabilities: &capabilities,
            policy: BatchPolicy::default(),
        },
    )
}

// ---------------------------------------------------------------------
// Publication granularity (tests 1-6)
// ---------------------------------------------------------------------

#[test]
fn two_resources_in_one_context_hold_independent_publications() {
    let fixture = Fixture::create("granular");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("A");
    refresh(&fixture, &backend(&fixture), "pkg/twin.py").expect("B");

    assert_eq!(state_of(&fixture, "pkg/impl.py"), SemanticState::Current);
    assert_eq!(state_of(&fixture, "pkg/twin.py"), SemanticState::Current);
    assert_eq!(
        count(&fixture, "semantic_publication"),
        2,
        "one row per owner, not one per context"
    );
    // Different generations, both current: sharing a Pyright process
    // is not sharing a currentness flag.
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let first = index
        .status(&owner_of(&fixture, "pkg/impl.py"))
        .expect("status")
        .stable_generation_id;
    let second = index
        .status(&owner_of(&fixture, "pkg/twin.py"))
        .expect("status")
        .stable_generation_id;
    assert_ne!(first, second);
}

#[test]
fn refreshing_one_owner_neither_revalidates_nor_dirties_another() {
    let fixture = Fixture::create("independent");
    refresh(&fixture, &backend(&fixture), "pkg/twin.py").expect("B");
    let before = status_of(&fixture, "pkg/twin.py");

    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("A");
    let after = status_of(&fixture, "pkg/twin.py");

    assert_eq!(before.state, SemanticState::Current);
    assert_eq!(after.state, SemanticState::Current);
    assert_eq!(
        before.stable_generation_id, after.stable_generation_id,
        "A's publication must not silently vouch for B"
    );
}

#[test]
fn an_owner_goes_dirty_only_when_its_own_basis_moves() {
    let fixture = Fixture::create("basis-scope");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("A");
    refresh(&fixture, &backend(&fixture), "pkg/twin.py").expect("B");

    // impl.py's basis names base.py; twin.py's does not.
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let touched = index
        .invalidate_resource(fixture.resource("pkg/base.py").id)
        .expect("invalidate");
    assert!(touched.contains(&owner_of(&fixture, "pkg/impl.py")));
    assert!(!touched.contains(&owner_of(&fixture, "pkg/twin.py")));

    assert_eq!(state_of(&fixture, "pkg/impl.py"), SemanticState::Dirty);
    assert_eq!(state_of(&fixture, "pkg/twin.py"), SemanticState::Current);
}

#[test]
fn coverage_reads_owner_level_currentness_not_context_level() {
    let fixture = Fixture::create("coverage-scope");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("A");
    refresh(&fixture, &backend(&fixture), "pkg/twin.py").expect("B");

    let store = GraphStore::open(&fixture.db_path()).expect("index.db");
    SemanticIndex::open(&fixture.db_path())
        .expect("index.db")
        .mark_dirty(&owner_of(&fixture, "pkg/impl.py"), BACKEND_UNAVAILABLE_CODE)
        .expect("dirty A");

    let dirty =
        crate::merge::semantic_scope(store.connection(), fixture.resource("pkg/impl.py").id)
            .expect("scope");
    let clean =
        crate::merge::semantic_scope(store.connection(), fixture.resource("pkg/twin.py").id)
            .expect("scope");
    assert!(dirty.not_current, "A's own contribution is not current");
    assert!(
        !clean.not_current,
        "B's is, and shares only the runtime with A"
    );
}

#[test]
fn a_contribution_with_no_owner_publication_never_reads_current() {
    let fixture = Fixture::create("no-publication");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("A");

    // What a migration from the old context-wide model leaves behind:
    // evidence with nothing vouching for it.
    let store = GraphStore::open(&fixture.db_path()).expect("index.db");
    store
        .connection()
        .execute(
            "DELETE FROM component_state WHERE component_kind = 'SEMANTIC_INDEX'",
            [],
        )
        .expect("drop the component rows");
    store
        .connection()
        .execute("DELETE FROM semantic_publication", [])
        .expect("drop the publications");

    assert_eq!(state_of(&fixture, "pkg/impl.py"), SemanticState::None);
    let scope =
        crate::merge::semantic_scope(store.connection(), fixture.resource("pkg/impl.py").id)
            .expect("scope");
    assert!(
        scope.contexts > 0 && scope.not_current,
        "leftover evidence reads NOT CURRENT, never CURRENT"
    );
}

// ---------------------------------------------------------------------
// Withdraw ordering (tests 7-12)
// ---------------------------------------------------------------------

#[test]
fn withdrawal_restores_gaps_and_lets_structural_replacement_run() {
    let fixture = Fixture::create("withdraw");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("published");
    let impl_run = symbol(&fixture, "pkg/impl.py", "Impl.run");
    assert_eq!(overrides_of(&fixture, impl_run).len(), 1);
    let gaps_before = list_unresolved_for_resource(
        GraphStore::open(&fixture.db_path())
            .expect("index.db")
            .connection(),
        fixture.resource("pkg/impl.py").id,
    )
    .expect("gaps")
    .len();

    // base.py is about to be re-extracted, and impl.py's contribution
    // points at its declarations.
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let affected: BTreeSet<SemanticOwner> = index
        .owners_depending_on(fixture.resource("pkg/base.py").id)
        .expect("owners")
        .into_iter()
        .collect();
    assert!(affected.contains(&owner_of(&fixture, "pkg/impl.py")));

    let report = lifecycle::withdraw_affected(&index, &affected, BACKEND_UNAVAILABLE_CODE)
        .expect("withdraw");
    assert!(report.gaps_restored > 0, "displaced gaps come back");
    assert!(report.relations_removed > 0, "semantic-only edges are GC'd");
    assert!(
        overrides_of(&fixture, symbol(&fixture, "pkg/impl.py", "Impl.run")).is_empty(),
        "the derived edge went with its proof"
    );
    let gaps_after = list_unresolved_for_resource(
        GraphStore::open(&fixture.db_path())
            .expect("index.db")
            .connection(),
        fixture.resource("pkg/impl.py").id,
    )
    .expect("gaps")
    .len();
    assert!(gaps_after >= gaps_before, "a silence would be dishonest");
    drop(index);

    // And only now can the structural replacement run at all.
    fs::write(
        fixture.root.join("pkg/base.py"),
        "class Base:\n    def renamed(self, value: int) -> str:\n        ...\n",
    )
    .expect("rewrite");
    BaselineScan::open(&fixture.db_path())
        .expect("index.db")
        .run_initial_scan(&fixture.root, &WorkspaceConfig::default(), "rev-2")
        .expect("structural replacement succeeds after withdrawal");
}

#[test]
fn withdrawing_one_owner_leaves_another_untouched() {
    let fixture = Fixture::create("withdraw-scope");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("A");
    refresh(&fixture, &backend(&fixture), "pkg/uses.py").expect("B");
    let evidence_before = count(&fixture, "semantic_evidence");

    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let only_a: BTreeSet<SemanticOwner> = [owner_of(&fixture, "pkg/impl.py")].into_iter().collect();
    lifecycle::withdraw_affected(&index, &only_a, BACKEND_UNAVAILABLE_CODE).expect("withdraw");

    assert_eq!(state_of(&fixture, "pkg/uses.py"), SemanticState::Current);
    assert!(count(&fixture, "semantic_evidence") < evidence_before);
    assert!(
        count(&fixture, "semantic_evidence") > 0,
        "B's evidence is still there"
    );
}

#[test]
fn withdrawal_keeps_every_structurally_proven_relation() {
    let fixture = Fixture::create("withdraw-structural");
    let structural = count(&fixture, "relation");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("published");

    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let affected: BTreeSet<SemanticOwner> =
        [owner_of(&fixture, "pkg/impl.py")].into_iter().collect();
    lifecycle::withdraw_affected(&index, &affected, BACKEND_UNAVAILABLE_CODE).expect("withdraw");

    assert_eq!(
        count(&fixture, "relation"),
        structural,
        "structural proof survives the backend's answer being withdrawn"
    );
}

// ---------------------------------------------------------------------
// Source save and dependency (tests 13-20)
// ---------------------------------------------------------------------

#[test]
fn a_dependent_owner_loses_its_derived_edge_and_regains_it_on_refresh() {
    let fixture = Fixture::create("dependent");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("published");
    let impl_run = symbol(&fixture, "pkg/impl.py", "Impl.run");
    assert_eq!(overrides_of(&fixture, impl_run).len(), 1);

    // Base.run disappears. impl.py's own bytes never change.
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let affected: BTreeSet<SemanticOwner> = index
        .owners_depending_on(fixture.resource("pkg/base.py").id)
        .expect("owners")
        .into_iter()
        .collect();
    lifecycle::withdraw_affected(&index, &affected, BACKEND_UNAVAILABLE_CODE).expect("withdraw");
    drop(index);
    assert_eq!(state_of(&fixture, "pkg/impl.py"), SemanticState::Dirty);

    fs::write(
        fixture.root.join("pkg/base.py"),
        "class Base:\n    def renamed(self, value: int) -> str:\n        ...\n",
    )
    .expect("rewrite");
    BaselineScan::open(&fixture.db_path())
        .expect("index.db")
        .run_initial_scan(&fixture.root, &WorkspaceConfig::default(), "rev-2")
        .expect("reindex");

    let quiet = ScriptedBackend::new().with_search_paths(Vec::new());
    refresh(&fixture, &quiet, "pkg/impl.py").expect("republished");
    assert!(
        overrides_of(&fixture, symbol(&fixture, "pkg/impl.py", "Impl.run")).is_empty(),
        "the old OVERRIDES cannot come back from a proof that is gone"
    );

    // Put it back, and a targeted refresh recreates the edge.
    fs::write(
        fixture.root.join("pkg/base.py"),
        "class Base:\n    def run(self, value: int) -> str:\n        ...\n",
    )
    .expect("restore");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let affected: BTreeSet<SemanticOwner> = index
        .owners_depending_on(fixture.resource("pkg/base.py").id)
        .expect("owners")
        .into_iter()
        .collect();
    lifecycle::withdraw_affected(&index, &affected, BACKEND_UNAVAILABLE_CODE).expect("withdraw");
    drop(index);
    BaselineScan::open(&fixture.db_path())
        .expect("index.db")
        .run_initial_scan(&fixture.root, &WorkspaceConfig::default(), "rev-3")
        .expect("reindex");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("republished");
    assert_eq!(
        overrides_of(&fixture, symbol(&fixture, "pkg/impl.py", "Impl.run")).len(),
        1,
        "targeted refresh rebuilds it"
    );

    // And the impact question answers from the refreshed graph.
    let base_run = GraphEndpoint::Symbol(symbol(&fixture, "pkg/base.py", "Base.run"));
    let impact = crate::impact::ImpactTraversal::open(&fixture.db_path())
        .expect("index.db")
        .run(
            crate::impact::ImpactIntent::BaseInterfaceChange,
            &GraphEndpoint::Symbol(symbol(&fixture, "pkg/base.py", "Base")),
            &crate::impact::Budget::default(),
        )
        .expect("impact");
    assert!(impact.nodes.iter().any(
        |node| node.endpoint == GraphEndpoint::Symbol(symbol(&fixture, "pkg/impl.py", "Impl"))
    ));
    let _ = base_run;
}

#[test]
fn an_unrelated_owner_is_never_in_the_affected_set() {
    let fixture = Fixture::create("unrelated");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("A");
    refresh(&fixture, &backend(&fixture), "pkg/twin.py").expect("B");

    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let config = lifecycle::PythonProjectConfig {
        source: ConfigSource::Defaults,
        resource: None,
    };
    let plan = lifecycle::plan_changes(
        &index,
        &context(),
        &[ResourceChange::new(
            fixture.resource("pkg/base.py").id,
            ChangeKind::Changed,
            "pkg/base.py",
        )],
        &config,
    )
    .expect("plan");
    assert!(plan.affected.contains(&owner_of(&fixture, "pkg/impl.py")));
    assert!(!plan.affected.contains(&owner_of(&fixture, "pkg/twin.py")));
    assert!(!plan.inventory_moved, "a content edit moves no module");
}

// ---------------------------------------------------------------------
// Inventory: ADD / DELETE / MOVE (tests 23-28)
// ---------------------------------------------------------------------

#[test]
fn adding_or_removing_a_module_reaches_every_owner_in_the_context() {
    let fixture = Fixture::create("inventory");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("A");
    refresh(&fixture, &backend(&fixture), "pkg/twin.py").expect("B");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let config = lifecycle::PythonProjectConfig {
        source: ConfigSource::Defaults,
        resource: None,
    };

    for kind in [ChangeKind::Added, ChangeKind::Deleted, ChangeKind::Moved] {
        let plan = lifecycle::plan_changes(
            &index,
            &context(),
            &[ResourceChange::new(
                fixture.resource("pkg/base.py").id,
                kind,
                "pkg/new_module.py",
            )],
            &config,
        )
        .expect("plan");
        assert!(plan.inventory_moved, "{kind:?} moves the module set");
        // Python resolves modules program-wide, so an untouched file's
        // imports can mean something else afterwards.
        assert!(plan.affected.contains(&owner_of(&fixture, "pkg/twin.py")));
    }
}

#[test]
fn a_non_python_change_does_not_move_the_python_inventory() {
    let fixture = Fixture::create("other-language");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("A");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let config = lifecycle::PythonProjectConfig {
        source: ConfigSource::Defaults,
        resource: None,
    };

    let plan = lifecycle::plan_changes(
        &index,
        &context(),
        &[ResourceChange::new(
            fixture.resource("pkg/base.py").id,
            ChangeKind::Added,
            "docs/readme.md",
        )
        .with_language(None)],
        &config,
    )
    .expect("plan");
    assert!(
        !plan.inventory_moved,
        "a markdown file is not a Python module"
    );
}

// ---------------------------------------------------------------------
// Backend notification (tests 29-32)
// ---------------------------------------------------------------------

#[test]
fn a_change_batch_becomes_one_deduped_watched_file_notification() {
    let fixture = Fixture::create("watched");
    let changes = vec![
        ResourceChange::new(
            fixture.resource("pkg/impl.py").id,
            ChangeKind::Changed,
            "pkg/impl.py",
        ),
        // The same file again, from a second owner's refresh.
        ResourceChange::new(
            fixture.resource("pkg/impl.py").id,
            ChangeKind::Changed,
            "pkg/impl.py",
        ),
        ResourceChange::new(
            fixture.resource("pkg/base.py").id,
            ChangeKind::Added,
            "pkg/added.py",
        ),
        ResourceChange::new(
            fixture.resource("pkg/twin.py").id,
            ChangeKind::Deleted,
            "pkg/gone.py",
        ),
        ResourceChange::new(
            fixture.resource("pkg/uses.py").id,
            ChangeKind::Moved,
            "pkg/new_name.py",
        )
        .from_previous("pkg/old_name.py"),
    ];

    let watched = lifecycle::watched_changes(&fixture.root, &changes);
    let kinds: Vec<(String, WatchedChangeKind)> = watched
        .iter()
        .map(|change| {
            (
                change.uri.rsplit('/').next().unwrap_or_default().to_owned(),
                change.kind,
            )
        })
        .collect();
    assert!(kinds.contains(&("impl.py".to_owned(), WatchedChangeKind::Changed)));
    assert!(kinds.contains(&("added.py".to_owned(), WatchedChangeKind::Created)));
    assert!(kinds.contains(&("gone.py".to_owned(), WatchedChangeKind::Deleted)));
    // A move is a delete of the old locator and a change at the new one.
    assert!(kinds.contains(&("old_name.py".to_owned(), WatchedChangeKind::Deleted)));
    assert!(kinds.contains(&("new_name.py".to_owned(), WatchedChangeKind::Changed)));
    assert_eq!(kinds.len(), 5, "the duplicate impl.py collapsed: {kinds:?}");

    // Deterministic order, so one Workspace operation is one batch.
    let again = lifecycle::watched_changes(&fixture.root, &changes);
    assert_eq!(watched, again);
}

#[test]
fn the_lifecycle_never_emits_a_document_overlay() {
    let fixture = Fixture::create("no-overlay");
    let scripted = backend(&fixture);
    refresh(&fixture, &scripted, "pkg/impl.py").expect("published");
    lifecycle_notify(&fixture, &scripted);

    for request in scripted.calls() {
        let (method, _) = request.wire();
        assert!(
            !protocol::FORBIDDEN_SYNC_METHODS.contains(&method),
            "{method} installs an editor overlay over Workspace truth"
        );
    }
    assert!(
        scripted
            .calls()
            .iter()
            .any(|request| matches!(request, PythonRequest::WatchedFilesChanged { .. })),
        "the filesystem change is announced the supported way"
    );
}

fn lifecycle_notify(fixture: &Fixture, queries: &dyn PythonQueries) {
    let changes = vec![ResourceChange::new(
        fixture.resource("pkg/impl.py").id,
        ChangeKind::Changed,
        "pkg/impl.py",
    )];
    adapter::notify_watched_files(queries, lifecycle::watched_changes(&fixture.root, &changes))
        .expect("notified");
}

// ---------------------------------------------------------------------
// Config (tests 33-38)
// ---------------------------------------------------------------------

#[test]
fn config_discovery_follows_the_backends_own_precedence() {
    let fixture = Fixture::create("config");
    let store = GraphStore::open(&fixture.db_path()).expect("index.db");

    // The committed fixture ships a pyrightconfig.json.
    let selected =
        lifecycle::discover_config(store.connection(), &fixture.root, "").expect("discover");
    assert_eq!(selected.source, ConfigSource::PyrightConfig);
    assert_eq!(
        selected.resource.as_ref().map(|r| r.path_key.as_str()),
        Some("pyrightconfig.json")
    );
}

#[test]
fn a_pyproject_without_a_pyright_table_is_not_the_config() {
    let fixture = Fixture::create("pyproject");
    fs::remove_file(fixture.root.join("pyrightconfig.json")).expect("remove");
    fixture.write("pyproject.toml", "[project]\nname = \"x\"\n");
    BaselineScan::open(&fixture.db_path())
        .expect("index.db")
        .run_initial_scan(&fixture.root, &WorkspaceConfig::default(), "rev-2")
        .expect("rescan");

    let store = GraphStore::open(&fixture.db_path()).expect("index.db");
    let selected =
        lifecycle::discover_config(store.connection(), &fixture.root, "").expect("discover");
    assert_eq!(
        selected.source,
        ConfigSource::Defaults,
        "a pyproject that configures something else is not the project config"
    );

    // Give it a [tool.pyright] table and it becomes the selection.
    fixture.write(
        "pyproject.toml",
        "[project]\nname = \"x\"\n\n[tool.pyright]\ntypeCheckingMode = \"strict\"\n",
    );
    BaselineScan::open(&fixture.db_path())
        .expect("index.db")
        .run_initial_scan(&fixture.root, &WorkspaceConfig::default(), "rev-3")
        .expect("rescan");
    let store = GraphStore::open(&fixture.db_path()).expect("index.db");
    let selected =
        lifecycle::discover_config(store.connection(), &fixture.root, "").expect("discover");
    assert_eq!(selected.source, ConfigSource::PyProject);
}

#[test]
fn changing_which_file_governs_changes_the_config_basis() {
    let fixture = Fixture::create("config-selection");
    let settings = PythonSettings::default();
    let store = GraphStore::open(&fixture.db_path()).expect("index.db");
    let with_pyright =
        lifecycle::discover_config(store.connection(), &fixture.root, "").expect("discover");
    drop(store);

    fs::remove_file(fixture.root.join("pyrightconfig.json")).expect("remove");
    fixture.write(
        "pyproject.toml",
        "[tool.pyright]\ntypeCheckingMode = \"strict\"\n",
    );
    BaselineScan::open(&fixture.db_path())
        .expect("index.db")
        .run_initial_scan(&fixture.root, &WorkspaceConfig::default(), "rev-2")
        .expect("rescan");
    let store = GraphStore::open(&fixture.db_path()).expect("index.db");
    let with_pyproject =
        lifecycle::discover_config(store.connection(), &fixture.root, "").expect("discover");

    assert_ne!(
        with_pyright.basis(&settings).fingerprint(),
        with_pyproject.basis(&settings).fingerprint(),
        "which file governs is part of the basis, not only its bytes"
    );
}

#[test]
fn a_config_change_dirties_every_owner_in_the_context() {
    let fixture = Fixture::create("config-change");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("A");
    refresh(&fixture, &backend(&fixture), "pkg/twin.py").expect("B");

    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let touched = lifecycle::invalidate_for_config(&index, &context()).expect("invalidate");
    assert_eq!(touched.len(), 2);
    drop(index);

    for rel in ["pkg/impl.py", "pkg/twin.py"] {
        assert_eq!(state_of(&fixture, rel), SemanticState::Dirty);
    }
    // And the graph still holds the structural truth.
    assert!(count(&fixture, "relation") > 0);
}

#[test]
fn an_unrelated_json_or_toml_change_is_not_a_config_change() {
    let fixture = Fixture::create("other-config");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("A");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let store = GraphStore::open(&fixture.db_path()).expect("index.db");
    let config =
        lifecycle::discover_config(store.connection(), &fixture.root, "").expect("discover");

    let plan = lifecycle::plan_changes(
        &index,
        &context(),
        &[ResourceChange::new(
            fixture.resource("pkg/twin.py").id,
            ChangeKind::Changed,
            "tools/other.json",
        )
        .with_language(None)],
        &config,
    )
    .expect("plan");
    assert!(!plan.config_moved, "only the selected config counts");
}

// ---------------------------------------------------------------------
// Environment / dependency (tests 39-43)
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// Environment helpers
// ---------------------------------------------------------------------

/// Write one ordinary wheel-style install: a `dist-info` directory with
/// the installer's `RECORD`.
fn install_distribution(site_packages: &std::path::Path, name_version: &str, record: &str) {
    let directory = site_packages.join(format!("{name_version}.dist-info"));
    fs::create_dir_all(&directory).expect("dist-info");
    fs::write(directory.join("RECORD"), record).expect("RECORD");
}

fn site_packages_of(fixture: &Fixture) -> std::path::PathBuf {
    let directory = fixture.root.join(".venv/lib/python3.12/site-packages");
    fs::create_dir_all(&directory).expect("site-packages");
    directory
}

fn venv_settings() -> PythonSettings {
    PythonSettings {
        python_path: Some(".venv/bin/python".to_owned()),
        venv_path: Some(".venv".to_owned()),
        ..PythonSettings::default()
    }
}

/// A venv this observer can actually prove: one readable distribution,
/// nothing that injects paths.
fn proven_environment(fixture: &Fixture) -> EnvironmentIdentity {
    install_distribution(
        &site_packages_of(fixture),
        "requests-2.31.0",
        "requests/__init__.py,,\n",
    );
    let identity = lifecycle::environment_identity(&fixture.root, &venv_settings());
    assert_eq!(
        identity.assurance,
        EnvironmentAssurance::Proven,
        "fixture venv should be provable"
    );
    identity
}

/// No venv at all, which is what most of these fixtures have.
fn unproven_environment(fixture: &Fixture) -> EnvironmentIdentity {
    let identity = lifecycle::environment_identity(&fixture.root, &PythonSettings::default());
    assert!(!identity.assurance.is_proven());
    identity
}

#[test]
fn the_environment_identity_covers_installed_distributions() {
    let fixture = Fixture::create("environment");
    let site_packages = site_packages_of(&fixture);
    install_distribution(
        &site_packages,
        "requests-2.31.0",
        "requests/__init__.py,,\n",
    );
    let settings = venv_settings();

    let before = lifecycle::environment_identity(&fixture.root, &settings);
    assert_eq!(before.assurance, EnvironmentAssurance::Proven);
    assert_eq!(before.distributions, 1, "diagnostic, not a proof state");
    // Deterministic, and unchanged metadata does not churn.
    assert_eq!(
        before.fingerprint,
        lifecycle::environment_identity(&fixture.root, &settings).fingerprint
    );

    // A dependency version moves without a line of project source
    // changing.
    fs::rename(
        site_packages.join("requests-2.31.0.dist-info"),
        site_packages.join("requests-2.32.0.dist-info"),
    )
    .expect("upgrade");
    let after = lifecycle::environment_identity(&fixture.root, &settings);
    assert_ne!(
        before.fingerprint, after.fingerprint,
        "an install can change what an import resolves to"
    );
    assert_eq!(after.assurance, EnvironmentAssurance::Proven);

    // And no dependency file became a Resource to notice it.
    assert!(
        fixture
            .resources()
            .iter()
            .all(|resource| !resource.path_key.contains("site-packages"))
    );
}

#[test]
fn distribution_order_does_not_change_the_fingerprint() {
    let first = Fixture::create("environment-order-a");
    let second = Fixture::create("environment-order-b");
    install_distribution(&site_packages_of(&first), "aaa-1.0", "aaa/__init__.py,,\n");
    install_distribution(&site_packages_of(&first), "zzz-1.0", "zzz/__init__.py,,\n");
    // Created in the opposite order, so any reliance on readdir order
    // shows up.
    install_distribution(&site_packages_of(&second), "zzz-1.0", "zzz/__init__.py,,\n");
    install_distribution(&site_packages_of(&second), "aaa-1.0", "aaa/__init__.py,,\n");

    assert_eq!(
        lifecycle::environment_identity(&first.root, &venv_settings()).fingerprint,
        lifecycle::environment_identity(&second.root, &venv_settings()).fingerprint,
        "the same installed set is the same environment"
    );
}

#[test]
fn a_same_version_reinstall_with_different_contents_moves_the_fingerprint() {
    let fixture = Fixture::create("environment-reinstall");
    let site_packages = site_packages_of(&fixture);
    install_distribution(&site_packages, "pkg-1.0", "pkg/__init__.py,sha256=aaa,10\n");
    let before = lifecycle::environment_identity(&fixture.root, &venv_settings());

    // Same name, same version, different installed content: only the
    // installer's RECORD says so.
    install_distribution(&site_packages, "pkg-1.0", "pkg/__init__.py,sha256=bbb,12\n");
    let after = lifecycle::environment_identity(&fixture.root, &venv_settings());

    assert_eq!(after.assurance, EnvironmentAssurance::Proven);
    assert_ne!(
        before.fingerprint, after.fingerprint,
        "a directory name is not an immutability proof; RECORD is"
    );
}

#[test]
fn an_unobservable_environment_is_unknown_rather_than_assumed_current() {
    let fixture = Fixture::create("environment-unknown");
    let identity = lifecycle::environment_identity(&fixture.root, &PythonSettings::default());
    assert!(matches!(
        identity.assurance,
        EnvironmentAssurance::Unknown { .. }
    ));
    assert!(!identity.assurance.is_proven());
    // Still deterministic: an unproven environment must not make every
    // publication churn.
    assert_eq!(
        identity.fingerprint,
        lifecycle::environment_identity(&fixture.root, &PythonSettings::default()).fingerprint
    );
}

#[test]
fn a_venv_without_site_packages_is_unknown() {
    let fixture = Fixture::create("environment-no-site-packages");
    fs::create_dir_all(fixture.root.join(".venv/bin")).expect("venv");
    let identity = lifecycle::environment_identity(&fixture.root, &venv_settings());
    assert!(!identity.assurance.is_proven());
    assert_eq!(identity.distributions, 0);
    assert_eq!(
        identity.fingerprint,
        lifecycle::environment_identity(&fixture.root, &venv_settings()).fingerprint
    );
}

/// The real filesystem, with one directory that refuses to be listed.
///
/// Permission bits are not usable here: CI frequently runs as root, for
/// which mode 0 is no obstacle at all.
struct UnreadableDir {
    path: std::path::PathBuf,
}

impl EnvironmentFs for UnreadableDir {
    fn read_dir(&self, path: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> {
        if path == self.path {
            return Err(std::io::Error::other("injected read failure"));
        }
        RealFs.read_dir(path)
    }

    fn read(&self, path: &std::path::Path) -> std::io::Result<Vec<u8>> {
        RealFs.read(path)
    }

    fn is_dir(&self, path: &std::path::Path) -> bool {
        RealFs.is_dir(path)
    }
}

#[test]
fn an_unreadable_site_packages_is_unknown_not_an_empty_environment() {
    let fixture = Fixture::create("environment-unreadable");
    let site_packages = site_packages_of(&fixture);
    install_distribution(
        &site_packages,
        "requests-2.31.0",
        "requests/__init__.py,,\n",
    );

    let filesystem = UnreadableDir {
        path: site_packages.clone(),
    };
    let identity =
        lifecycle::environment_identity_with(&fixture.root, &venv_settings(), &filesystem);

    assert!(
        !identity.assurance.is_proven(),
        "an I/O error is not an observation: {:?}",
        identity.assurance
    );
    assert_eq!(
        identity.distributions, 0,
        "and zero distributions here means nothing was seen, not that none exist"
    );
    assert_ne!(
        identity.fingerprint,
        lifecycle::environment_identity(&fixture.root, &venv_settings()).fingerprint,
        "what was read and what could not be are different observations"
    );
}

#[test]
fn a_distribution_whose_record_cannot_be_read_is_unknown() {
    let fixture = Fixture::create("environment-unreadable-record");
    let site_packages = site_packages_of(&fixture);
    // A dist-info with no RECORD at all: readable directory, no proof.
    fs::create_dir_all(site_packages.join("pkg-1.0.dist-info")).expect("dist-info");

    let identity = lifecycle::environment_identity(&fixture.root, &venv_settings());
    assert!(!identity.assurance.is_proven());
    assert_eq!(identity.distributions, 1, "it was seen, just not proven");
}

#[test]
fn an_editable_install_is_unknown_however_stable_its_version_looks() {
    let fixture = Fixture::create("environment-editable");
    let site_packages = site_packages_of(&fixture);
    install_distribution(&site_packages, "my-lib-1.0", "my_lib/__init__.py,,\n");
    fs::write(
        site_packages.join("my-lib-1.0.dist-info/direct_url.json"),
        r#"{"url":"file:///work/my-lib","dir_info":{"editable":true}}"#,
    )
    .expect("direct_url.json");

    let identity = lifecycle::environment_identity(&fixture.root, &venv_settings());
    assert!(
        !identity.assurance.is_proven(),
        "the checkout can change with no installed metadata moving"
    );
    // And the source checkout was neither walked nor indexed.
    assert!(
        fixture
            .resources()
            .iter()
            .all(|resource| !resource.path_key.contains("site-packages"))
    );
}

#[test]
fn a_non_editable_directory_install_stays_proven() {
    let fixture = Fixture::create("environment-local-copy");
    let site_packages = site_packages_of(&fixture);
    install_distribution(&site_packages, "my-lib-1.0", "my_lib/__init__.py,,\n");
    fs::write(
        site_packages.join("my-lib-1.0.dist-info/direct_url.json"),
        r#"{"url":"file:///work/my-lib","dir_info":{}}"#,
    )
    .expect("direct_url.json");

    // A copy, and RECORD pins what was copied.
    let identity = lifecycle::environment_identity(&fixture.root, &venv_settings());
    assert_eq!(identity.assurance, EnvironmentAssurance::Proven);
}

#[test]
fn an_egg_link_environment_is_not_silently_proven() {
    let fixture = Fixture::create("environment-egg-link");
    let site_packages = site_packages_of(&fixture);
    install_distribution(
        &site_packages,
        "requests-2.31.0",
        "requests/__init__.py,,\n",
    );
    fs::write(site_packages.join("my-lib.egg-link"), "/work/my-lib\n").expect("egg-link");

    assert!(
        !lifecycle::environment_identity(&fixture.root, &venv_settings())
            .assurance
            .is_proven()
    );
}

#[test]
fn a_path_injecting_pth_is_not_silently_proven() {
    let fixture = Fixture::create("environment-pth");
    let site_packages = site_packages_of(&fixture);
    install_distribution(
        &site_packages,
        "requests-2.31.0",
        "requests/__init__.py,,\n",
    );

    // A comment-only .pth changes nothing about resolution.
    fs::write(site_packages.join("harmless.pth"), "# nothing here\n").expect("pth");
    assert_eq!(
        lifecycle::environment_identity(&fixture.root, &venv_settings()).assurance,
        EnvironmentAssurance::Proven
    );

    // One that adds a directory to sys.path does.
    fs::write(site_packages.join("inject.pth"), "/work/src\n").expect("pth");
    let injected = lifecycle::environment_identity(&fixture.root, &venv_settings());
    assert!(!injected.assurance.is_proven());
    assert_eq!(
        injected.fingerprint,
        lifecycle::environment_identity(&fixture.root, &venv_settings()).fingerprint,
        "unknown does not mean random"
    );
}

#[test]
fn a_legacy_egg_info_install_is_not_silently_proven() {
    let fixture = Fixture::create("environment-egg-info");
    let site_packages = site_packages_of(&fixture);
    fs::create_dir_all(site_packages.join("old-1.0.egg-info")).expect("egg-info");

    assert!(
        !lifecycle::environment_identity(&fixture.root, &venv_settings())
            .assurance
            .is_proven()
    );
}

// ---------------------------------------------------------------------
// Environment assurance in revalidation
// ---------------------------------------------------------------------

#[test]
fn a_reopen_under_a_proven_unchanged_environment_restores_current_without_a_backend() {
    let fixture = Fixture::create("assurance-proven-reopen");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("published");
    let environment = proven_environment(&fixture);

    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let inventory =
        inventory_fingerprint(index.connection(), ResourceLanguage::Python).expect("inventory");
    let answers = lifecycle::revalidate_context(
        &index,
        &context(),
        &lifecycle::current_inputs(
            &context(),
            &config_basis(&PythonSettings::default(), None),
            &capability_report(&context()),
            &inventory,
            &environment,
        ),
    )
    .expect("revalidate");

    assert!(answers.iter().all(|(_, status)| status.is_current()));
}

#[test]
fn a_reopen_under_an_unknown_environment_cannot_restore_current() {
    let fixture = Fixture::create("assurance-unknown-reopen");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("published");
    let environment = unproven_environment(&fixture);

    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let inventory =
        inventory_fingerprint(index.connection(), ResourceLanguage::Python).expect("inventory");
    let inputs = lifecycle::current_inputs(
        &context(),
        &config_basis(&PythonSettings::default(), None),
        &capability_report(&context()),
        &inventory,
        &environment,
    );
    // Every other input compares equal; only the assurance differs.
    assert!(
        inputs
            .clone()
            .with_environment_proven(true)
            .environment_fingerprint
            == inputs.environment_fingerprint
    );
    let answers = lifecycle::revalidate_context(&index, &context(), &inputs).expect("revalidate");

    let (_, status) = &answers[0];
    assert_eq!(status.state, SemanticState::Dirty);
    assert_eq!(
        status.last_error_code.as_deref(),
        Some(ENVIRONMENT_UNPROVEN_CODE)
    );
    assert!(status.has_last_valid(), "the last valid answer is kept");
}

#[test]
fn a_live_refresh_under_an_unknown_environment_still_publishes_current() {
    let fixture = Fixture::create("assurance-unknown-refresh");
    assert!(!unproven_environment(&fixture).assurance.is_proven());

    // The backend just analyzed the filesystem as it is, so the result
    // describes the live environment whatever can be proven later.
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("published");
    assert_eq!(state_of(&fixture, "pkg/impl.py"), SemanticState::Current);

    // And the very next reopen owes a refresh, because that is the
    // claim that cannot be proven from persisted metadata alone.
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let inventory =
        inventory_fingerprint(index.connection(), ResourceLanguage::Python).expect("inventory");
    let answers = lifecycle::revalidate_context(
        &index,
        &context(),
        &lifecycle::current_inputs(
            &context(),
            &config_basis(&PythonSettings::default(), None),
            &capability_report(&context()),
            &inventory,
            &unproven_environment(&fixture),
        ),
    )
    .expect("revalidate");
    assert_eq!(answers[0].1.state, SemanticState::Dirty);
}

#[test]
fn a_changed_proven_environment_invalidates_only_the_owner_publications() {
    let fixture = Fixture::create("assurance-env-changed");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("published");
    let before = proven_environment(&fixture);
    let relations = count(&fixture, "relation");

    // An install, with no project source touched.
    install_distribution(
        &site_packages_of(&fixture),
        "extra-1.0",
        "extra/__init__.py,,\n",
    );
    let after = lifecycle::environment_identity(&fixture.root, &venv_settings());
    assert_eq!(after.assurance, EnvironmentAssurance::Proven);
    assert_ne!(before.fingerprint, after.fingerprint);

    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let inventory =
        inventory_fingerprint(index.connection(), ResourceLanguage::Python).expect("inventory");
    let mut inputs = lifecycle::current_inputs(
        &context(),
        &config_basis(&PythonSettings::default(), None),
        &capability_report(&context()),
        &inventory,
        &after,
    );
    inputs.environment_fingerprint = after.fingerprint.clone();
    let answers = lifecycle::revalidate_context(&index, &context(), &inputs).expect("revalidate");
    drop(index);

    assert_eq!(answers[0].1.state, SemanticState::Dirty);
    assert_eq!(
        answers[0].1.last_error_code.as_deref(),
        Some(ENVIRONMENT_CHANGED_CODE)
    );
    assert_eq!(
        count(&fixture, "relation"),
        relations,
        "structural Workspace truth is not a semantic publication"
    );
}

// ---------------------------------------------------------------------
// Missing / incompatible backend (tests 44-48)
// ---------------------------------------------------------------------

struct AbsentBackend;

impl SemanticBackendLauncher for AbsentBackend {
    fn kind(&self) -> SemanticBackendKind {
        SemanticBackendKind::Python
    }
    fn launch(
        &self,
        _binding: &AnalysisContextBinding,
    ) -> Result<Arc<dyn SemanticRuntimeHost>, HostError> {
        Err(HostError::new("no pyright-typeserver install"))
    }
}

#[test]
fn a_missing_backend_leaves_structure_current_and_semantics_incomplete() {
    let fixture = Fixture::create("no-backend");
    let supervisor = SemanticRuntimeSupervisor::new(RuntimePolicy::default())
        .with_backend(Arc::new(AbsentBackend));
    let binding = AnalysisContextBinding {
        context: context(),
        project_root_rel: String::new(),
        config_file_rel: None,
    };
    assert!(matches!(
        supervisor.acquire(&binding),
        Err(StartFailure::Backend(_))
    ));

    // Structural discovery, symbols and relations are all there.
    assert!(fixture.resources().len() > 5);
    assert!(count(&fixture, "symbol") > 0);
    assert!(count(&fixture, "relation") > 0);
    assert_eq!(
        count(&fixture, "semantic_publication"),
        0,
        "no empty success"
    );

    // And the semantic-required sites keep the answer incomplete.
    let store = GraphStore::open(&fixture.db_path()).expect("index.db");
    let gaps = list_unresolved_for_resource(store.connection(), fixture.resource("pkg/impl.py").id)
        .expect("gaps");
    assert!(!gaps.is_empty());

    let answer = RelationIndex::open(&fixture.db_path())
        .expect("index.db")
        .outgoing(
            &GraphEndpoint::Symbol(symbol(&fixture, "pkg/inherit.py", "Qualified")),
            &[RelationKind::Extends],
        )
        .expect("outgoing");
    assert_eq!(answer.confirmed_count(), 0);
    assert!(
        !answer.coverage.limits().is_complete(),
        "zero with complete coverage would be the false zero"
    );
}

#[test]
fn availability_separates_a_cold_runtime_from_a_stale_answer() {
    let current = SemanticStatus {
        state: SemanticState::Current,
        support: Some(Support::Supported),
        stable_generation_id: Some(1),
        basis: None,
        last_error_code: None,
    };
    let dirty = SemanticStatus {
        state: SemanticState::Dirty,
        ..current.clone()
    };

    // A crash does not invalidate a proof whose inputs still hold.
    assert_eq!(
        lifecycle::availability(&current, RuntimeState::Cold, BackendReadiness::Available),
        SemanticAvailability::CurrentRuntimeCold
    );
    assert_eq!(
        lifecycle::availability(&current, RuntimeState::Ready, BackendReadiness::Available),
        SemanticAvailability::Current
    );
    // No install at all: a current publication is still current, it
    // just cannot be renewed.
    assert_eq!(
        lifecycle::availability(&current, RuntimeState::Cold, BackendReadiness::Unavailable),
        SemanticAvailability::CurrentRuntimeCold
    );
    // An input moved and the backend cannot refresh: not current, and
    // not pretending otherwise.
    assert_eq!(
        lifecycle::availability(&dirty, RuntimeState::Cold, BackendReadiness::Unavailable),
        SemanticAvailability::Unavailable
    );
    assert_eq!(
        lifecycle::availability(&dirty, RuntimeState::Ready, BackendReadiness::Available),
        SemanticAvailability::RefreshRequired
    );
    assert!(!SemanticAvailability::RefreshRequired.serves_current());
    assert!(SemanticAvailability::CurrentRuntimeCold.serves_current());

    // A partial answer is served, and says so.
    let partial = SemanticStatus {
        support: Some(Support::Partial),
        ..current
    };
    assert_eq!(
        lifecycle::availability(&partial, RuntimeState::Ready, BackendReadiness::Available),
        SemanticAvailability::Partial
    );
}

#[test]
fn a_capability_report_does_not_shrink_because_a_process_is_missing() {
    // Implementation support is a product statement; a dead process is
    // a runtime one. Collapsing them would turn a crash into a claim
    // that Brainprint cannot resolve references at all.
    let report = capability_report(&context());
    assert_eq!(
        report.support(crate::semantic::SemanticCapability::References),
        Support::Supported
    );
    assert_eq!(
        lifecycle::availability(
            &SemanticStatus::none(),
            RuntimeState::Cold,
            BackendReadiness::Unavailable
        ),
        SemanticAvailability::Unavailable,
        "availability is where the outage shows up"
    );
}

// ---------------------------------------------------------------------
// Crash and failure (tests 49-53, 60-62)
// ---------------------------------------------------------------------

#[test]
fn a_crash_with_an_unchanged_basis_keeps_the_publication_current() {
    let fixture = Fixture::create("crash-valid");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("published");
    assert_eq!(state_of(&fixture, "pkg/impl.py"), SemanticState::Current);

    // The process dies. Nothing about the Workspace changed, and the
    // environment is still there to be read.
    let environment = proven_environment(&fixture);
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let status = index
        .revalidate(
            &owner_of(&fixture, "pkg/impl.py"),
            &lifecycle::current_inputs(
                &context(),
                &config_basis(&PythonSettings::default(), None),
                &capability_report(&context()),
                &inventory_fingerprint(index.connection(), ResourceLanguage::Python)
                    .expect("inventory"),
                &environment,
            ),
        )
        .expect("revalidate");
    assert!(
        status.is_current(),
        "a dead process is not a reason to disbelieve a proof: {status:?}"
    );
}

#[test]
fn a_crash_after_an_input_moved_does_not_leave_the_old_fact_current() {
    let fixture = Fixture::create("crash-stale");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("published");

    // base.py moves, then the backend dies before anything is refreshed.
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let affected: BTreeSet<SemanticOwner> = index
        .owners_depending_on(fixture.resource("pkg/base.py").id)
        .expect("owners")
        .into_iter()
        .collect();
    lifecycle::withdraw_affected(&index, &affected, BACKEND_UNAVAILABLE_CODE).expect("withdraw");
    lifecycle::mark_unavailable(&index, &affected, BACKEND_UNAVAILABLE_CODE).expect("unavailable");
    drop(index);

    let status = status_of(&fixture, "pkg/impl.py");
    assert_eq!(status.state, SemanticState::Unavailable);
    assert!(status.has_last_valid(), "the last valid answer is kept");
    assert!(
        overrides_of(&fixture, symbol(&fixture, "pkg/impl.py", "Impl.run")).is_empty(),
        "the semantic-only edge went with the withdrawn proof"
    );
    assert!(count(&fixture, "relation") > 0, "structural truth remains");
}

#[test]
fn a_failed_refresh_keeps_the_last_valid_publication_and_marks_the_owner() {
    let fixture = Fixture::create("refresh-failed");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("published");
    let published = status_of(&fixture, "pkg/impl.py");

    let failing = ScriptedBackend::new().failing(crate::runtime::RequestFailure::TimedOut {
        after: Duration::from_millis(5),
    });
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let context = context();
    let capabilities = capability_report(&context);
    let config = config_basis(&PythonSettings::default(), None);
    let mut request = RefreshRequest {
        context: &context,
        workspace_root: &fixture.root,
        owner: fixture.resource("pkg/impl.py").id,
        config: &config,
        capabilities: &capabilities,
        policy: BatchPolicy::default(),
    };
    let owners: BTreeSet<SemanticOwner> = [owner_of(&fixture, "pkg/impl.py")].into_iter().collect();
    let outcomes =
        lifecycle::refresh_owners(&index, &failing, &mut request, &owners).expect("refresh pass");

    assert_eq!(outcomes.len(), 1);
    assert!(!outcomes[0].succeeded());
    drop(index);
    let after = status_of(&fixture, "pkg/impl.py");
    assert_eq!(after.state, SemanticState::Dirty);
    assert_eq!(
        after.stable_generation_id, published.stable_generation_id,
        "a failure never replaces a real answer with an empty success"
    );
}

#[test]
fn one_owners_failure_does_not_erase_another_owners_publication() {
    let fixture = Fixture::create("partial-failure");
    refresh(&fixture, &backend(&fixture), "pkg/twin.py").expect("B");

    let failing = ScriptedBackend::new().failing(crate::runtime::RequestFailure::TimedOut {
        after: Duration::from_millis(5),
    });
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let context = context();
    let capabilities = capability_report(&context);
    let config = config_basis(&PythonSettings::default(), None);
    let mut request = RefreshRequest {
        context: &context,
        workspace_root: &fixture.root,
        owner: fixture.resource("pkg/impl.py").id,
        config: &config,
        capabilities: &capabilities,
        policy: BatchPolicy::default(),
    };
    let owners: BTreeSet<SemanticOwner> = [owner_of(&fixture, "pkg/impl.py")].into_iter().collect();
    lifecycle::refresh_owners(&index, &failing, &mut request, &owners).expect("refresh pass");
    drop(index);

    assert_eq!(state_of(&fixture, "pkg/twin.py"), SemanticState::Current);
}

// ---------------------------------------------------------------------
// Daemon reopen and reconcile (tests 54-59)
// ---------------------------------------------------------------------

#[test]
fn a_reopen_with_an_unchanged_basis_stays_current_without_a_backend() {
    let fixture = Fixture::create("reopen");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("A");
    refresh(&fixture, &backend(&fixture), "pkg/twin.py").expect("B");
    let environment = proven_environment(&fixture);

    // A fresh SemanticIndex, exactly as a restarted daemon opens it.
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let inventory =
        inventory_fingerprint(index.connection(), ResourceLanguage::Python).expect("inventory");
    let answers = lifecycle::revalidate_context(
        &index,
        &context(),
        &lifecycle::current_inputs(
            &context(),
            &config_basis(&PythonSettings::default(), None),
            &capability_report(&context()),
            &inventory,
            &environment,
        ),
    )
    .expect("revalidate");

    assert_eq!(answers.len(), 2);
    assert!(
        answers.iter().all(|(_, status)| status.is_current()),
        "nothing moved, so nothing needed a process to prove it"
    );
}

#[test]
fn a_reopen_after_an_input_moved_reports_dirty_before_any_launch() {
    let fixture = Fixture::create("reopen-dirty");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("A");

    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let inventory =
        inventory_fingerprint(index.connection(), ResourceLanguage::Python).expect("inventory");
    // A different config fingerprint is all it takes.
    let moved = config_basis(
        &PythonSettings {
            type_checking_mode: Some("strict".to_owned()),
            ..PythonSettings::default()
        },
        None,
    );
    let answers = lifecycle::revalidate_context(
        &index,
        &context(),
        &lifecycle::current_inputs(
            &context(),
            &moved,
            &capability_report(&context()),
            &inventory,
            &proven_environment(&fixture),
        ),
    )
    .expect("revalidate");

    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].1.state, SemanticState::Dirty);
    assert!(answers[0].1.has_last_valid());
}

#[test]
fn a_missed_change_recovered_later_invalidates_the_same_owners() {
    let fixture = Fixture::create("reconcile");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("A");
    refresh(&fixture, &backend(&fixture), "pkg/twin.py").expect("B");

    // Reconcile discovers the change a watcher missed. The plan is the
    // same one a live event would have produced.
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let config = lifecycle::PythonProjectConfig {
        source: ConfigSource::Defaults,
        resource: None,
    };
    let change = ResourceChange::new(
        fixture.resource("pkg/base.py").id,
        ChangeKind::Changed,
        "pkg/base.py",
    );
    let live = lifecycle::plan_changes(&index, &context(), std::slice::from_ref(&change), &config)
        .expect("plan");
    let recovered =
        lifecycle::plan_changes(&index, &context(), &[change], &config).expect("plan again");
    assert_eq!(live.affected, recovered.affected);
    assert!(live.affected.contains(&owner_of(&fixture, "pkg/impl.py")));

    // Applying it twice changes nothing the second time.
    lifecycle::withdraw_affected(&index, &live.affected, BACKEND_UNAVAILABLE_CODE)
        .expect("withdraw");
    let evidence = count(&fixture, "semantic_evidence");
    let relations = count(&fixture, "relation");
    lifecycle::withdraw_affected(&index, &recovered.affected, BACKEND_UNAVAILABLE_CODE)
        .expect("withdraw again");
    assert_eq!(count(&fixture, "semantic_evidence"), evidence);
    assert_eq!(count(&fixture, "relation"), relations);
}

#[test]
fn a_structural_dependent_is_withdrawn_before_the_replacement_that_re_resolves_it() {
    // #19 task 9 acceptance found this: the affected set was computed
    // from the semantic basis alone, but a structural publication also
    // re-resolves every Resource whose relations point into the one
    // that changed. Those owners' evidence hangs off edges the
    // re-resolution removes, and `semantic_evidence.relation_id` has no
    // cascade -- so an incomplete withdrawal is not a stale row, it is
    // a failed publication.
    let fixture = Fixture::create("structural-dependents");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("impl");
    // imports.py imports `Base` -- a relation I3 already resolved
    // structurally, so base.py never enters imports.py's *semantic*
    // basis, yet a base.py replacement re-resolves it.
    refresh(&fixture, &backend(&fixture), "pkg/imports.py").expect("imports");

    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let base = fixture.resource("pkg/base.py");
    let change = ResourceChange::new(base.id, ChangeKind::Changed, "pkg/base.py");
    let plan = lifecycle::plan_changes(
        &index,
        &context(),
        std::slice::from_ref(&change),
        &lifecycle::PythonProjectConfig {
            source: ConfigSource::Defaults,
            resource: None,
        },
    )
    .expect("plan");

    // The structural tier's own answer to "who gets re-resolved".
    let dependents = crate::graph_lifecycle::dependents_of(index.connection(), &[base.id], false)
        .expect("dependents");
    assert!(
        dependents.len() > 1,
        "the fixture must have a dependent beyond the changed file: {dependents:?}"
    );
    for dependent in dependents {
        let owner = SemanticOwner::new(context().context_key(), dependent);
        if index
            .status(&owner)
            .expect("status")
            .stable_generation_id
            .is_some()
        {
            assert!(
                plan.affected.contains(&owner),
                "{owner} is re-resolved but was not planned for withdrawal"
            );
        }
    }

    // And the whole ordering actually completes: withdraw, then let the
    // structural replacement run.
    lifecycle::withdraw_affected(&index, &plan.affected, BACKEND_UNAVAILABLE_CODE)
        .expect("withdraw");
    fixture.write(
        "pkg/base.py",
        "class Base:\n    def run(self, value: int) -> str:\n        return \"\"\n",
    );
    drop(index);
    crate::reconcile::Reconcile::open(&fixture.db_path())
        .expect("index.db")
        .run(&fixture.root, &WorkspaceConfig::default())
        .expect("the structural replacement must not hit a foreign key");
}

#[test]
fn a_plan_over_no_changes_is_empty_and_idempotent() {
    let fixture = Fixture::create("no-changes");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("A");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let config = lifecycle::PythonProjectConfig {
        source: ConfigSource::Defaults,
        resource: None,
    };
    let plan = lifecycle::plan_changes(&index, &context(), &[], &config).expect("plan");
    assert!(plan.is_empty());
    drop(index);
    assert_eq!(state_of(&fixture, "pkg/impl.py"), SemanticState::Current);
}

// ---------------------------------------------------------------------
// Queries during degradation (tests 63-68)
// ---------------------------------------------------------------------

#[test]
fn queries_keep_structural_truth_and_refuse_stale_semantic_truth() {
    let fixture = Fixture::create("degraded-queries");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("published");
    let base_run = GraphEndpoint::Symbol(symbol(&fixture, "pkg/base.py", "Base.run"));
    let callers_before = RelationIndex::open(&fixture.db_path())
        .expect("index.db")
        .callers(&base_run)
        .expect("callers")
        .confirmed_count();
    assert!(callers_before > 0);

    // The backend goes away and impl.py's contribution is withdrawn.
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let affected: BTreeSet<SemanticOwner> =
        [owner_of(&fixture, "pkg/impl.py")].into_iter().collect();
    lifecycle::withdraw_affected(&index, &affected, BACKEND_UNAVAILABLE_CODE).expect("withdraw");
    drop(index);

    // The semantic-only caller is gone rather than presented as clean.
    let relations = RelationIndex::open(&fixture.db_path()).expect("index.db");
    let after = relations.callers(&base_run).expect("callers");
    assert!(after.confirmed_count() < callers_before);
    assert!(
        !after.gaps.is_empty() || !after.coverage.limits().is_complete(),
        "the site that used to resolve is an honest gap now"
    );

    // Structural relations elsewhere are untouched, and prepared
    // inspection still reads the current file.
    assert!(count(&fixture, "relation") > 0);
    let prepared = crate::prepare::InspectPreparer::open(&fixture.db_path(), &fixture.root)
        .expect("preparer")
        .prepare(
            &GraphEndpoint::Symbol(symbol(&fixture, "pkg/base.py", "Base")),
            crate::relations::Direction::Incoming,
            &[RelationKind::Extends],
        )
        .expect("prepared");
    assert!(
        prepared.ranges.iter().all(|range| range.source
            == fixture.text(&range.path_rel)[range.span.start_byte..range.span.end_byte]),
        "prepared source is the current filesystem source"
    );
}

// ---------------------------------------------------------------------
// Isolation and idempotence (tests 69-71)
// ---------------------------------------------------------------------

#[test]
fn another_worktree_cannot_touch_this_ones_semantic_state() {
    let fixture = Fixture::create("worktree-a");
    let other = Fixture::create("worktree-b");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("A");
    refresh(&other, &backend(&other), "pkg/impl.py").expect("B");

    let index = SemanticIndex::open(&other.db_path()).expect("index.db");
    let affected: BTreeSet<SemanticOwner> = index
        .owners_depending_on(other.resource("pkg/base.py").id)
        .expect("owners")
        .into_iter()
        .collect();
    lifecycle::withdraw_affected(&index, &affected, BACKEND_UNAVAILABLE_CODE).expect("withdraw");
    drop(index);

    assert_eq!(
        state_of(&fixture, "pkg/impl.py"),
        SemanticState::Current,
        "a separate index is a separate world"
    );
    assert_eq!(state_of(&other, "pkg/impl.py"), SemanticState::Dirty);
}

#[test]
fn the_whole_lifecycle_is_idempotent_over_an_unchanged_workspace() {
    let fixture = Fixture::create("lifecycle-idempotent");
    refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("first");
    let relations = count(&fixture, "relation");
    let evidence = count(&fixture, "semantic_evidence");
    let publications = count(&fixture, "semantic_publication");
    let conflicts = count(&fixture, "semantic_conflict");

    for _ in 0..3 {
        refresh(&fixture, &backend(&fixture), "pkg/impl.py").expect("again");
    }
    assert_eq!(count(&fixture, "relation"), relations);
    assert_eq!(count(&fixture, "semantic_evidence"), evidence);
    assert_eq!(count(&fixture, "semantic_publication"), publications);
    assert_eq!(count(&fixture, "semantic_conflict"), conflicts);
    assert_eq!(state_of(&fixture, "pkg/impl.py"), SemanticState::Current);
}
