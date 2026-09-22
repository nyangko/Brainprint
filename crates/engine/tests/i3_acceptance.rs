//! I3 end-to-end acceptance (#17 task 15).
//!
//! Tasks 1-14 each proved their own piece. This proves they compose:
//! one real Workspace, scanned through the real baseline scan, then
//! driven through relation extraction, resolution, direct query,
//! current-source preparation, typed impact, related-test projection,
//! coverage, and the whole refresh/delete/move/reconcile lifecycle --
//! through the same public paths a caller would use.
//!
//! The representative Workspace is the I0 `python-signature-impact`
//! fixture, copied so each test owns its own `index.db`. It is the same
//! snapshot the I0 `basic-tools` baseline and the I2 re-measurement
//! used, which is what makes the benchmark comparison meaningful.
//!
//! No new feature is exercised here. Where a scenario needs state a
//! fixture cannot produce on its own (a DIRTY relation component, a
//! container-only Resource), the state is set the way the owning task
//! sets it and the query path is the real one.

use std::{
    collections::{BTreeSet, HashMap},
    env, fs,
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use brainprint_core::{ResourceId, SymbolId};
use brainprint_engine::{
    component,
    config::WorkspaceConfig,
    coverage::{AnswerState, CoverageLimit},
    graph::{GraphEndpoint, GraphStore, RelationKind},
    impact::{Budget, ImpactIntent, ImpactTraversal, Truncation},
    prepare::InspectPreparer,
    reconcile::Reconcile,
    refresh::TargetedRefresh,
    related_tests::{ProjectionOutcome, RelatedTests},
    relations::{Direction, RelationAnswer, RelationIndex},
    resolution::{Freshness, Resolution, Support},
    resource::{Resource, ResourceRole, ResourceStore},
    scan::BaselineScan,
    structural,
    symbol::{OccurrenceKind, Symbol, SymbolStore},
    watch::{RawWatchEvent, WatchIngest},
};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

const DEFINITION: &str = "src/profile_app/profile.py";
const SERVICE: &str = "src/profile_app/service.py";
const ADMIN: &str = "src/profile_app/admin.py";
const CONFIG: &str = "src/profile_app/config.py";
const TEST: &str = "tests/test_profile.py";
const PACKAGE_INIT: &str = "src/profile_app/__init__.py";

/// The representative Workspace, copied and indexed.
struct Fixture {
    base: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn open(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let base = env::temp_dir().join(format!(
            "brainprint-i3-{label}-{}-{sequence}",
            process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("workspace");
        copy_tree(&representative_fixture(), &root).expect("copy the fixture");
        let fixture = Self { base, root };
        fixture.baseline();
        fixture
    }

    fn baseline(&self) {
        BaselineScan::open(&self.db_path())
            .expect("index.db")
            .run_initial_scan(&self.root, &WorkspaceConfig::default(), "i3-rev-1")
            .expect("baseline scan");
    }

    fn db_path(&self) -> PathBuf {
        self.base.join("data").join("index.db")
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn write(&self, rel: &str, contents: &str) {
        if let Some(parent) = self.path(rel).parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(self.path(rel), contents).expect("write");
    }

    /// One saved file, through the watcher and the targeted refresh.
    fn save(&self, rel: &str, contents: &str) {
        self.write(rel, contents);
        self.ingest(&[RawWatchEvent::Modified {
            path: self.path(rel),
        }]);
        TargetedRefresh::open(&self.db_path())
            .expect("index.db")
            .run(&self.root, &WorkspaceConfig::default())
            .expect("refresh");
    }

    fn ingest(&self, events: &[RawWatchEvent]) {
        WatchIngest::open(&self.db_path())
            .expect("index.db")
            .ingest_all(&self.root, &WorkspaceConfig::default(), events)
            .expect("ingest");
    }

    fn reconcile(&self) {
        Reconcile::open(&self.db_path())
            .expect("index.db")
            .run(&self.root, &WorkspaceConfig::default())
            .expect("reconcile");
    }

    fn resources(&self) -> ResourceStore {
        ResourceStore::open(&self.db_path()).expect("index.db")
    }

    fn resource(&self, rel: &str) -> Resource {
        self.try_resource(rel)
            .unwrap_or_else(|| panic!("{rel} is an active Resource"))
    }

    fn try_resource(&self, rel: &str) -> Option<Resource> {
        self.resources()
            .get_active_by_path_key(rel)
            .expect("lookup")
    }

    fn file(&self, rel: &str) -> GraphEndpoint {
        GraphEndpoint::Resource(self.resource(rel).id)
    }

    fn symbols(&self, rel: &str) -> Vec<Symbol> {
        SymbolStore::open(&self.db_path())
            .expect("index.db")
            .list_for_resource(self.resource(rel).id)
            .expect("symbols")
    }

    fn symbol(&self, rel: &str, qualified_name: &str) -> SymbolId {
        self.symbols(rel)
            .into_iter()
            .find(|symbol| symbol.qualified_name == qualified_name)
            .unwrap_or_else(|| panic!("{qualified_name} is indexed in {rel}"))
            .id
    }

    fn declaration(&self, rel: &str, qualified_name: &str) -> GraphEndpoint {
        GraphEndpoint::Symbol(self.symbol(rel, qualified_name))
    }

    /// The representative target: `build_profile`.
    fn target(&self) -> GraphEndpoint {
        self.declaration(DEFINITION, "build_profile")
    }

    fn index(&self) -> RelationIndex {
        RelationIndex::open(&self.db_path()).expect("index.db")
    }

    fn store(&self) -> GraphStore {
        GraphStore::open(&self.db_path()).expect("index.db")
    }

    fn traversal(&self) -> ImpactTraversal {
        ImpactTraversal::open(&self.db_path()).expect("index.db")
    }

    fn tests(&self) -> RelatedTests {
        RelatedTests::open(&self.db_path()).expect("index.db")
    }

    fn preparer(&self) -> InspectPreparer {
        InspectPreparer::open(&self.db_path(), &self.root).expect("index.db")
    }

    fn outgoing(&self, endpoint: &GraphEndpoint, kinds: &[RelationKind]) -> RelationAnswer {
        self.index().outgoing(endpoint, kinds).expect("query")
    }

    fn incoming(&self, endpoint: &GraphEndpoint, kinds: &[RelationKind]) -> RelationAnswer {
        self.index().incoming(endpoint, kinds).expect("query")
    }

    /// Which Resources own each confirmed caller's evidence.
    fn caller_files(&self, target: &GraphEndpoint) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        for relation in &self.incoming(target, &[RelationKind::Calls]).confirmed {
            for evidence in &relation.evidence {
                found.insert(self.path_of(evidence.resource));
            }
        }
        found
    }

    fn path_of(&self, resource: ResourceId) -> String {
        self.resources()
            .get_by_id(resource)
            .expect("lookup")
            .expect("the Resource exists")
            .path_rel
    }

    /// Every (kind, source, target) triple stored, as printable text.
    fn all_relation_keys(&self) -> Vec<String> {
        let store = self.store();
        let connection = store.connection();
        let mut statement = connection
            .prepare(
                "SELECT relation.kind, relation.source_entity_id, relation.target_entity_id \
                 FROM relation ORDER BY relation.kind, relation.source_entity_id, \
                 relation.target_entity_id",
            )
            .expect("statement");
        let rows = statement
            .query_map([], |row| {
                Ok(format!(
                    "{}:{}:{}",
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?
                ))
            })
            .expect("query");
        rows.map(|row| row.expect("row")).collect()
    }

    fn count(&self, sql: &str) -> i64 {
        let store = self.store();
        store
            .connection()
            .query_row(sql, [], |row| row.get::<_, i64>(0))
            .expect("count")
    }

    /// Every active Resource's `(path, revision)`, for change isolation.
    fn revisions(&self) -> HashMap<String, String> {
        let store = self.store();
        let connection = store.connection();
        let mut statement = connection
            .prepare("SELECT path_rel, resource_revision FROM resource WHERE state = 'ACTIVE'")
            .expect("statement");
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query");
        rows.map(|row| row.expect("row")).collect()
    }

    /// Set one Resource's `RELATION_INDEX` state, as task 13 does.
    fn set_relation_state(&self, rel: &str, state: &str) {
        let store = self.store();
        let updated = store
            .connection()
            .execute(
                "UPDATE component_state SET freshness_state = ?1 \
                 WHERE component_kind = ?2 AND scope_key = ?3",
                rusqlite::params![
                    state,
                    component::RELATION_INDEX,
                    structural::scope_key(self.resource(rel).id)
                ],
            )
            .expect("component state");
        assert_eq!(updated, 1, "{rel} has a RELATION_INDEX row");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn representative_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/workspaces/python-signature-impact")
        .canonicalize()
        .expect("the representative fixture is in the repository")
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn generous() -> Budget {
    Budget {
        max_nodes: 200,
        max_edges: 200,
        max_depth: 10,
        time_limit: Duration::from_secs(30),
    }
}

// =================================================================
// 1. The representative scenario, answered without text fallback.
// =================================================================

#[test]
fn the_representative_change_is_answered_from_the_graph_alone() {
    let fixture = Fixture::open("representative");
    let target = fixture.target();

    // One prepared inspection: the definition's current source, every
    // confirmed caller, each caller's exact evidence span, and the
    // declaration each span sits in.
    let prepared = fixture
        .preparer()
        .prepare(&target, Direction::Incoming, &[RelationKind::Calls])
        .expect("prepare");

    let anchor = prepared
        .target
        .as_ref()
        .expect("the definition is a Symbol");
    assert_eq!(anchor.path_rel, DEFINITION);
    let definition_source = prepared
        .range(anchor.range.expect("the definition source was prepared"))
        .expect("range");
    assert!(
        definition_source.source.starts_with("def build_profile("),
        "the definition arrived as source, not as a line number: {:?}",
        definition_source.source
    );

    assert_eq!(prepared.confirmed_count(), 3, "three confirmed callers");
    let mut callers: Vec<String> = Vec::new();
    for relation in &prepared.relations {
        for evidence in &relation.evidence {
            let range = prepared
                .range(evidence.evidence_range.expect("evidence source"))
                .expect("range");
            assert_eq!(range.source, "build_profile");
            assert!(
                evidence
                    .containing_range
                    .and_then(|id| prepared.range(id))
                    .is_some_and(|range| range.source.starts_with("def ")),
                "the caller's own declaration was prepared"
            );
            callers.push(range.path_rel.clone());
        }
    }
    callers.sort();
    assert_eq!(callers, [ADMIN, SERVICE, TEST]);
    assert!(prepared.source_complete());

    // The related test comes from the same graph, not from its name.
    let related = fixture
        .tests()
        .for_target(&target, ImpactIntent::PublicSignatureChange, &generous())
        .expect("projection");
    assert_eq!(
        related
            .candidates
            .iter()
            .map(|candidate| candidate.path_rel.as_str())
            .collect::<Vec<_>>(),
        [TEST]
    );
    assert_eq!(related.outcome(), ProjectionOutcome::Candidates);

    // And the coverage behind all of it is stated rather than implied.
    let answer = fixture.incoming(&target, &[RelationKind::Calls]);
    assert_eq!(answer.answer_state(), AnswerState::Confirmed);
    assert!(
        answer
            .coverage
            .limits()
            .has(CoverageLimit::ReverseScopeNotEnumerable)
    );
}

// =================================================================
// 2. caller / callee / import / importer / reference / type baseline.
// =================================================================

#[test]
fn the_direct_relation_baseline_is_confirmed_with_exact_evidence() {
    let fixture = Fixture::open("direct");
    let target = fixture.target();

    // Callers, by stable identity, each with an exact span.
    let callers = fixture.incoming(&target, &[RelationKind::Calls]);
    assert_eq!(callers.confirmed_count(), 3);
    for relation in &callers.confirmed {
        assert_eq!(relation.resolution, Resolution::Resolved);
        assert!(matches!(relation.source, GraphEndpoint::Symbol(_)));
        assert_eq!(relation.target, target);
        assert_eq!(relation.evidence.len(), 1);
        let evidence = &relation.evidence[0];
        assert_eq!(evidence.occurrence_kind, OccurrenceKind::CallSite);
        assert!(evidence.span.end_byte > evidence.span.start_byte);
        assert!(evidence.span.start.line > 0);
        assert_eq!(evidence.support, Support::Supported);
        assert_eq!(evidence.freshness, Freshness::Fresh);
    }

    // Callees, read from the other end of the same rows.
    let callees = fixture.outgoing(
        &fixture.declaration(SERVICE, "render_user"),
        &[RelationKind::Calls],
    );
    assert_eq!(callees.confirmed_count(), 1);
    assert_eq!(callees.confirmed[0].target, target);

    // Imports and importers.
    let imports = fixture
        .index()
        .imports(fixture.resource(SERVICE).id)
        .expect("query");
    assert_eq!(imports.confirmed_count(), 1);
    assert_eq!(imports.confirmed[0].target, fixture.file(DEFINITION));
    let importers = fixture
        .index()
        .importers(&fixture.file(DEFINITION))
        .expect("query");
    let mut importing: Vec<String> = importers
        .confirmed
        .iter()
        .map(|relation| match relation.source {
            GraphEndpoint::Resource(id) => fixture.path_of(id),
            ref other => panic!("an import came from {other:?}"),
        })
        .collect();
    importing.sort();
    assert_eq!(importing, [ADMIN, SERVICE, TEST]);

    // A call site is a CALLS and is not also a generic REFERENCES.
    let references = fixture.index().references(&target).expect("query");
    assert_eq!(
        references.confirmed_count(),
        0,
        "a call was duplicated as a reference"
    );

    // Deterministic ordering, and the same answer every time.
    let repeated = fixture.incoming(&target, &[RelationKind::Calls]);
    assert_eq!(callers.confirmed, repeated.confirmed);
    assert_eq!(callers.coverage, repeated.coverage);

    // No reverse relation rows exist to make any of this work.
    assert_eq!(
        fixture.count(
            "SELECT COUNT(*) FROM relation WHERE kind IN \
             ('CALLED_BY', 'IMPORTED_BY', 'REFERENCED_BY', 'TESTED_BY', 'RELATED_TEST')"
        ),
        0
    );
}

// =================================================================
// 3. unresolved / candidate / unsupported survive the lifecycle.
// =================================================================

#[test]
fn unresolved_and_unsupported_evidence_survive_and_stay_unresolved() {
    let fixture = Fixture::open("unresolved");

    // `os.getenv(...)`: the receiver's type decides the callee, and no
    // semantic backend exists to say so. It stays a gap.
    let config = fixture.outgoing(&fixture.declaration(CONFIG, "PROFILE_LOCALE"), &[]);
    let receiver = config
        .gaps
        .iter()
        .find(|gap| gap.lookup_name == "getenv")
        .expect("the os.getenv call site is preserved as a gap");
    assert!(receiver.reason.requires_semantics());
    assert_eq!(receiver.resolution, Resolution::Unresolved);
    assert!(receiver.candidates.is_empty(), "no candidate was invented");

    // A builtin type annotation this tier does not model stays a gap
    // rather than resolving to something same-named.
    let definition = fixture.outgoing(&fixture.target(), &[]);
    let builtin = definition
        .gaps
        .iter()
        .find(|gap| gap.lookup_name == "str")
        .expect("the unresolved annotation is preserved");
    assert_eq!(builtin.resolution, Resolution::Unresolved);
    assert_eq!(
        definition.answer_state(),
        AnswerState::NoneWithIncompleteCoverage,
        "zero confirmed outgoing relations with a gap present is not a negative"
    );

    // Every gap is anchored to a real Occurrence, and no gap shares an
    // Occurrence with a confirmed relation.
    assert_eq!(
        fixture.count(
            "SELECT COUNT(*) FROM unresolved_reference \
             JOIN occurrence ON occurrence.id = unresolved_reference.occurrence_id \
             WHERE occurrence.relation_id IS NOT NULL"
        ),
        0
    );
    // Nothing was promoted: a candidate row never became a relation.
    assert_eq!(
        fixture.count(
            "SELECT COUNT(*) FROM relation_candidate \
             WHERE unresolved_reference_id NOT IN (SELECT id FROM unresolved_reference)"
        ),
        0
    );
}

// =================================================================
// 4. Change-oriented current source preparation.
// =================================================================

#[test]
fn preparation_returns_current_source_rather_than_line_numbers() {
    let fixture = Fixture::open("prepare");
    let target = fixture.target();

    let prepared = fixture
        .preparer()
        .prepare(&target, Direction::Incoming, &[RelationKind::Calls])
        .expect("prepare");

    // Seven distinct ranges: the anchor declaration, and an evidence
    // span plus its containing declaration for each of three callers.
    assert_eq!(prepared.ranges.len(), 7);
    for range in &prepared.ranges {
        assert!(!range.source.is_empty());
        let current = fs::read_to_string(fixture.path(&range.path_rel)).expect("current source");
        assert_eq!(
            &current[range.span.start_byte..range.span.end_byte],
            range.source,
            "a prepared range did not match the file's current bytes"
        );
    }
    assert!(prepared.source_complete());
    assert!(prepared.currentness.is_current());
    assert_eq!(prepared.answer_state(), AnswerState::Confirmed);

    // And the index still holds no source body of its own: the
    // prepared bytes came from the filesystem, not from `index.db`.
    let store = fixture.store();
    let mut tables = store
        .connection()
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .expect("statement");
    let names: Vec<String> = tables
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query")
        .map(|row| row.expect("row"))
        .collect();
    for table in names {
        let mut columns = store
            .connection()
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .expect("statement");
        let column_names: Vec<String> = columns
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .map(|row| row.expect("row"))
            .collect();
        for column in column_names {
            assert!(
                !matches!(
                    column.as_str(),
                    "source" | "source_text" | "body" | "snippet" | "content" | "text"
                ),
                "{table}.{column} looks like a source body mirror"
            );
        }
    }
}

// =================================================================
// 5. Typed impact.
// =================================================================

#[test]
fn typed_impact_walks_only_the_intended_kinds() {
    let fixture = Fixture::open("impact");
    let target = fixture.target();
    let traversal = fixture.traversal();

    let signature = traversal
        .run(ImpactIntent::PublicSignatureChange, &target, &generous())
        .expect("impact");
    assert_eq!(signature.edges.len(), 3);
    assert!(signature.edges.iter().all(|edge| {
        ImpactIntent::PublicSignatureChange
            .kinds()
            .contains(&edge.relation.kind)
    }));
    assert!(
        signature
            .edges
            .iter()
            .all(|edge| edge.relation.direction == Direction::Incoming),
        "impact followed an outgoing edge"
    );
    assert!(signature.is_complete_walk());

    let rename = traversal
        .run(ImpactIntent::Rename, &target, &generous())
        .expect("impact");
    assert_eq!(rename.edges.len(), 3);

    // A module move is a Resource question, and the importers answer it.
    let module_move = traversal
        .run(
            ImpactIntent::ModuleMove,
            &fixture.file(DEFINITION),
            &generous(),
        )
        .expect("impact");
    assert_eq!(module_move.edges.len(), 3);
    assert!(
        module_move
            .edges
            .iter()
            .all(|edge| edge.relation.kind == RelationKind::Imports)
    );

    // This fixture has no inheritance, and that answer is honest about
    // the unresolved evidence around it rather than claiming zero.
    let base = traversal
        .run(ImpactIntent::BaseInterfaceChange, &target, &generous())
        .expect("impact");
    assert_eq!(base.edges.len(), 0);
    assert_eq!(base.answer_state(), AnswerState::NoneWithIncompleteCoverage);

    // Every endpoint is expanded once and every canonical edge emitted
    // once, so a cycle could not run away and a diamond is not doubled.
    let mut seen = BTreeSet::new();
    for edge in &signature.edges {
        assert!(
            seen.insert(format!(
                "{}:{:?}:{:?}",
                edge.relation.kind.as_str(),
                edge.relation.source,
                edge.relation.target
            )),
            "an edge was emitted twice"
        );
    }
    let mut nodes = BTreeSet::new();
    for node in &signature.nodes {
        assert!(
            nodes.insert(format!("{:?}", node.endpoint)),
            "a node repeated"
        );
    }

    // Deterministic: the same walk twice is the same walk.
    let again = traversal
        .run(ImpactIntent::PublicSignatureChange, &target, &generous())
        .expect("impact");
    assert_eq!(signature.nodes, again.nodes);
    assert_eq!(signature.edges, again.edges);
}

#[test]
fn a_truncated_impact_continues_and_degrades_its_coverage() {
    let fixture = Fixture::open("impact-budget");
    let target = fixture.target();
    let traversal = fixture.traversal();

    let tight = Budget {
        max_edges: 1,
        ..generous()
    };
    let first = traversal
        .run(ImpactIntent::PublicSignatureChange, &target, &tight)
        .expect("impact");

    assert_eq!(first.truncation, Some(Truncation::EdgeBudget));
    assert!(!first.coverage.is_complete());
    assert!(first.limits().has(CoverageLimit::TraversalTruncated));
    let continuation = first.continuation.clone().expect("a continuation");

    let resumed = traversal
        .resume(
            ImpactIntent::PublicSignatureChange,
            &target,
            &continuation,
            &generous(),
        )
        .expect("resume");
    let total = first.edges.len() + resumed.edges.len();
    assert_eq!(total, 3, "resuming completed the walk exactly once");
    for edge in &resumed.edges {
        assert!(
            !first.edges.contains(edge),
            "a resumed walk repeated an edge"
        );
    }
}

// =================================================================
// 6. Related tests.
// =================================================================

#[test]
fn related_tests_come_from_test_role_and_confirmed_paths_only() {
    let fixture = Fixture::open("related-tests");
    let target = fixture.target();

    assert_eq!(fixture.resource(TEST).role, ResourceRole::Test);
    let projection = fixture
        .tests()
        .for_target(&target, ImpactIntent::PublicSignatureChange, &generous())
        .expect("projection");

    assert_eq!(projection.candidates.len(), 1);
    let candidate = &projection.candidates[0];
    assert_eq!(candidate.path_rel, TEST);
    assert_eq!(candidate.distance, 1, "a direct confirmed call");
    assert!(candidate.basis.is_direct());
    assert!(!candidate.paths.is_empty());
    assert!(
        candidate.paths[0]
            .hops
            .iter()
            .all(|hop| hop.resolution == Resolution::Resolved),
        "a related test was justified by something unconfirmed"
    );

    // A same-named file with no confirmed relation is not a candidate.
    fixture.write(
        "tests/test_build_profile_lookalike.py",
        "def test_build_profile_lookalike() -> None:\n    assert True\n",
    );
    fixture.ingest(&[RawWatchEvent::Created {
        path: fixture.path("tests/test_build_profile_lookalike.py"),
    }]);
    fixture.reconcile();
    let after = fixture
        .tests()
        .for_target(&target, ImpactIntent::PublicSignatureChange, &generous())
        .expect("projection");
    assert_eq!(
        after
            .candidates
            .iter()
            .map(|candidate| candidate.path_rel.as_str())
            .collect::<Vec<_>>(),
        [TEST],
        "a filename lookalike became a related test"
    );

    // And no relation family was persisted to make this work.
    assert_eq!(
        fixture.count(
            "SELECT COUNT(*) FROM relation WHERE kind IN ('RELATED_TEST', 'TESTED_BY', 'TESTS')"
        ),
        0
    );
}

// =================================================================
// 7. USES_ENV / USES_CONFIG.
// =================================================================

#[test]
fn env_relations_round_trip_and_dynamic_keys_stay_gaps() {
    let fixture = Fixture::open("env");
    let owner = fixture.declaration(CONFIG, "PROFILE_LOCALE");

    let uses = fixture.outgoing(&owner, &[RelationKind::UsesEnv]);
    assert_eq!(uses.confirmed_count(), 1);
    let relation = &uses.confirmed[0];
    let GraphEndpoint::Domain(entity) = &relation.target else {
        panic!("the env key is not a DomainEntity: {:?}", relation.target);
    };
    assert_eq!(entity.kind, "ENV");
    assert_eq!(entity.normalized_identity, "PROFILE_LOCALE");
    assert_eq!(relation.evidence.len(), 1);
    assert_eq!(
        relation.evidence[0].occurrence_kind,
        OccurrenceKind::KeySite
    );

    // Reverse: who uses this key.
    let users = fixture.incoming(&relation.target, &[RelationKind::UsesEnv]);
    assert_eq!(users.confirmed_count(), 1);
    assert_eq!(users.confirmed[0].source, owner);

    // A dynamic key names nothing, invents nothing, and prevents the
    // zero next to it from looking complete.
    fixture.save(
        CONFIG,
        "import os\n\nPROFILE_LOCALE = os.getenv(\"PROFILE_LOCALE\", \"en\")\n\n\ndef lookup(name: str) -> str | None:\n    return os.getenv(name)\n",
    );
    let dynamic = fixture.outgoing(
        &fixture.declaration(CONFIG, "lookup"),
        &[RelationKind::UsesEnv],
    );
    assert_eq!(dynamic.confirmed_count(), 0);
    assert_eq!(
        dynamic
            .gaps
            .iter()
            .map(|gap| gap.reason.as_str())
            .collect::<Vec<_>>(),
        ["DYNAMIC_KEY_EXPRESSION"]
    );
    assert_eq!(
        dynamic.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM domain_entity WHERE normalized_identity = 'name'"),
        0,
        "a dynamic key became an entity"
    );
}

// =================================================================
// 8. Targeted refresh.
// =================================================================

#[test]
fn a_targeted_refresh_touches_only_the_saved_resource_and_its_dependents() {
    let fixture = Fixture::open("refresh");
    let target = fixture.target();
    let before = fixture.revisions();

    // One caller stops calling, and gains an import it did not have.
    fixture.save(
        ADMIN,
        "from .profile import build_profile\nfrom .config import PROFILE_LOCALE\n\n\ndef admin_preview(user_id: str) -> dict[str, str]:\n    return {\"user_id\": user_id, \"locale\": PROFILE_LOCALE}\n",
    );

    let after = fixture.revisions();
    for (path, revision) in &before {
        if path == ADMIN {
            assert_ne!(Some(revision), after.get(path), "the saved file moved");
        } else {
            assert_eq!(
                Some(revision),
                after.get(path),
                "{path} was re-published by an unrelated save"
            );
        }
    }

    // The removed relation is gone, the added one is there, and the
    // other owners' evidence survived.
    let callers = fixture.incoming(&target, &[RelationKind::Calls]);
    assert_eq!(callers.confirmed_count(), 2);
    assert_eq!(
        fixture.caller_files(&target),
        [SERVICE.to_owned(), TEST.to_owned()].into_iter().collect()
    );
    let imports = fixture
        .index()
        .imports(fixture.resource(ADMIN).id)
        .expect("query");
    let mut imported: Vec<String> = imports
        .confirmed
        .iter()
        .map(|relation| match relation.target {
            GraphEndpoint::Resource(id) => fixture.path_of(id),
            ref other => panic!("{other:?}"),
        })
        .collect();
    imported.sort();
    assert_eq!(imported, [CONFIG, DEFINITION]);

    // The canonical CALLS edge admin -> build_profile lost its last
    // evidence and was collected.
    assert!(
        !fixture.caller_files(&target).contains(ADMIN),
        "a relation outlived its last evidence"
    );
}

// =================================================================
// 9. Target-definition change.
// =================================================================

#[test]
fn dependents_move_between_resolved_and_unresolved_when_the_target_changes() {
    let fixture = Fixture::open("target-change");
    let target = fixture.target();
    assert_eq!(
        fixture
            .incoming(&target, &[RelationKind::Calls])
            .confirmed_count(),
        3
    );

    // The definition is renamed. Every dependent still names the old
    // one, which no longer exists.
    fixture.save(
        DEFINITION,
        "from .config import PROFILE_LOCALE\n\n\ndef assemble_profile(user_id: str, locale: str = PROFILE_LOCALE) -> dict[str, str]:\n    return {\"user_id\": user_id, \"locale\": locale}\n",
    );

    let service = fixture.outgoing(
        &fixture.declaration(SERVICE, "render_user"),
        &[RelationKind::Calls],
    );
    assert_eq!(service.confirmed_count(), 0);
    assert_eq!(
        service.answer_state(),
        AnswerState::NoneWithIncompleteCoverage,
        "a vanished target left a complete-looking zero"
    );
    assert!(
        service
            .gaps
            .iter()
            .any(|gap| gap.lookup_name == "build_profile"),
        "the dependent's call site was discarded instead of becoming a gap"
    );
    // No stale RESOLVED relation was left behind claiming freshness.
    assert_eq!(
        fixture.count(
            "SELECT COUNT(*) FROM symbol WHERE qualified_name = 'build_profile' \
             AND resource_id = (SELECT id FROM resource WHERE path_key = 'src/profile_app/profile.py')"
        ),
        0
    );

    // Putting it back re-resolves the same dependents.
    fixture.save(
        DEFINITION,
        "from .config import PROFILE_LOCALE\n\n\ndef build_profile(user_id: str, locale: str = PROFILE_LOCALE) -> dict[str, str]:\n    return {\"user_id\": user_id, \"locale\": locale}\n",
    );
    let restored = fixture.target();
    assert_eq!(
        fixture
            .incoming(&restored, &[RelationKind::Calls])
            .confirmed_count(),
        3,
        "dependents did not recover after the target came back"
    );
}

// =================================================================
// 10. DELETE.
// =================================================================

#[test]
fn deleting_a_resource_removes_its_evidence_and_invalidates_its_dependents() {
    let fixture = Fixture::open("delete");
    let service_file = fixture.file(SERVICE);
    let definition_file = fixture.file(DEFINITION);
    let owned_gaps_before = fixture.count(
        "SELECT COUNT(*) FROM unresolved_reference \
         JOIN occurrence ON occurrence.id = unresolved_reference.occurrence_id \
         JOIN resource ON resource.id = occurrence.resource_id \
         WHERE resource.path_key = 'src/profile_app/service.py'",
    );
    assert!(owned_gaps_before > 0);

    fs::remove_file(fixture.path(SERVICE)).expect("remove");
    fixture.ingest(&[RawWatchEvent::Removed {
        path: fixture.path(SERVICE),
    }]);
    fixture.reconcile();

    // Gone from the current index, and nothing points at it.
    assert!(fixture.try_resource(SERVICE).is_none());
    assert_eq!(
        fixture.count(
            "SELECT COUNT(*) FROM unresolved_reference \
             JOIN occurrence ON occurrence.id = unresolved_reference.occurrence_id \
             JOIN resource ON resource.id = occurrence.resource_id \
             WHERE resource.path_key = 'src/profile_app/service.py'"
        ),
        0
    );
    let importers = fixture.index().importers(&definition_file).expect("query");
    assert!(
        importers
            .confirmed
            .iter()
            .all(|relation| relation.source != service_file),
        "a deleted Resource is still exposed as a current importer"
    );

    // The other owners' evidence survived.
    assert_eq!(
        fixture.caller_files(&fixture.target()),
        [ADMIN.to_owned(), TEST.to_owned()].into_iter().collect()
    );
}

#[test]
fn deleting_a_target_invalidates_its_dependents_without_a_false_zero() {
    let fixture = Fixture::open("delete-target");

    fs::remove_file(fixture.path(DEFINITION)).expect("remove");
    fixture.ingest(&[RawWatchEvent::Removed {
        path: fixture.path(DEFINITION),
    }]);
    fixture.reconcile();

    let service = fixture.outgoing(
        &fixture.declaration(SERVICE, "render_user"),
        &[RelationKind::Calls],
    );
    assert_eq!(service.confirmed_count(), 0);
    assert_eq!(
        service.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );

    let imports = fixture
        .index()
        .imports(fixture.resource(SERVICE).id)
        .expect("query");
    assert_eq!(imports.confirmed_count(), 0);
    assert!(
        imports
            .gaps
            .iter()
            .any(|gap| gap.lookup_name.contains("profile")),
        "the import of a deleted module vanished instead of becoming a gap"
    );
}

// =================================================================
// 11. Identity-preserved MOVE.
// =================================================================

#[test]
fn an_identity_preserving_move_keeps_the_resource_and_updates_the_locator() {
    let fixture = Fixture::open("move");
    let before = fixture.resource(TEST);
    let moved = "tests/test_profile_renamed.py";

    fs::rename(fixture.path(TEST), fixture.path(moved)).expect("rename");
    fixture.ingest(&[RawWatchEvent::RenamedPair {
        from: fixture.path(TEST),
        to: fixture.path(moved),
    }]);
    fixture.reconcile();

    let after = fixture.resource(moved);
    assert_eq!(
        after.id, before.id,
        "the ResourceId did not survive the move"
    );
    assert_eq!(after.path_rel, moved);
    assert!(
        fixture.try_resource(TEST).is_none(),
        "the old locator is current"
    );

    // Unrelated relations are untouched, and the moved file still
    // proves the same edge from its new location.
    let callers = fixture.caller_files(&fixture.target());
    assert_eq!(
        callers,
        [ADMIN.to_owned(), SERVICE.to_owned(), moved.to_owned()]
            .into_iter()
            .collect()
    );
    let related = fixture
        .tests()
        .for_target(
            &fixture.target(),
            ImpactIntent::PublicSignatureChange,
            &generous(),
        )
        .expect("projection");
    assert_eq!(
        related
            .candidates
            .iter()
            .map(|candidate| candidate.path_rel.as_str())
            .collect::<Vec<_>>(),
        [moved]
    );
}

// =================================================================
// 12. Watcher loss / reconcile.
// =================================================================

#[test]
fn reconcile_recovers_missed_changes_and_leaves_the_rest_alone() {
    let fixture = Fixture::open("reconcile");
    let before = fixture.revisions();

    // Changes the watcher never saw: one edit, one delete.
    fixture.write(
        ADMIN,
        "from .profile import build_profile\n\n\ndef admin_preview(user_id: str) -> dict[str, str]:\n    return build_profile(user_id, locale=\"en\")\n\n\ndef admin_default(user_id: str) -> dict[str, str]:\n    return build_profile(user_id)\n",
    );
    fs::remove_file(fixture.path(SERVICE)).expect("remove");
    fixture.ingest(&[RawWatchEvent::ContinuityLost {
        detail: "test".to_owned(),
    }]);
    fixture.reconcile();

    let after = fixture.revisions();
    assert!(!after.contains_key(SERVICE));
    assert_ne!(before.get(ADMIN), after.get(ADMIN));
    for path in [DEFINITION, CONFIG, TEST, PACKAGE_INIT] {
        assert_eq!(
            before.get(path),
            after.get(path),
            "{path} was re-published by a reconcile that did not change it"
        );
    }

    // The recovered state is coherent: two call sites from admin, one
    // canonical relation, and no stale service evidence left current.
    // Two caller Symbols in admin plus the test: three canonical
    // relations, and admin's two call sites are two evidence spans on
    // two distinct source Symbols, not duplicates of one edge.
    let callers = fixture.incoming(&fixture.target(), &[RelationKind::Calls]);
    assert_eq!(callers.confirmed_count(), 3);
    let admin_evidence: usize = callers
        .confirmed
        .iter()
        .flat_map(|relation| &relation.evidence)
        .filter(|evidence| fixture.path_of(evidence.resource) == ADMIN)
        .count();
    assert_eq!(admin_evidence, 2, "both recovered call sites are evidence");
    assert!(
        callers
            .confirmed
            .iter()
            .all(|relation| relation.freshness == Freshness::Fresh)
    );

    // A second reconcile changes nothing.
    let graph = fixture.all_relation_keys();
    fixture.reconcile();
    assert_eq!(fixture.revisions(), after, "reconcile is not idempotent");
    assert_eq!(fixture.all_relation_keys(), graph);
}

// =================================================================
// 13. Coherent visibility.
// =================================================================

#[test]
fn every_published_relation_is_coherent_with_its_owners_current_revision() {
    let fixture = Fixture::open("coherent");

    let checkpoints: [&dyn Fn(); 0] = [];
    let _ = checkpoints;
    for step in 0..3 {
        match step {
            1 => fixture.save(
                SERVICE,
                "from .profile import build_profile\n\n\ndef render_user(user_id: str) -> str:\n    return str(build_profile(user_id))\n",
            ),
            2 => fixture.reconcile(),
            _ => {}
        }

        // No evidence row describes a revision its Resource has left,
        // which is what a half-replaced publication would look like.
        assert_eq!(
            fixture.count(
                "SELECT COUNT(*) FROM occurrence \
                 JOIN resource ON resource.id = occurrence.resource_id \
                 WHERE resource.state = 'ACTIVE' \
                   AND occurrence.resource_revision <> resource.resource_revision"
            ),
            0,
            "step {step}: an Occurrence describes an older revision than its Resource"
        );
        // No canonical relation is left without evidence.
        assert_eq!(
            fixture.count(
                "SELECT COUNT(*) FROM relation WHERE id NOT IN \
                 (SELECT relation_id FROM occurrence WHERE relation_id IS NOT NULL)"
            ),
            0,
            "step {step}: a relation outlived its evidence"
        );
        // And every confirmed answer reads as current.
        let callers = fixture.incoming(&fixture.target(), &[RelationKind::Calls]);
        assert!(
            callers
                .confirmed
                .iter()
                .all(|relation| relation.freshness == Freshness::Fresh
                    && relation.support == Support::Supported),
            "step {step}: a published relation was not current"
        );
    }
}

// =================================================================
// 14. Workspace isolation.
// =================================================================

#[test]
fn two_workspaces_with_the_same_paths_do_not_share_graph_state() {
    let left = Fixture::open("isolation-left");
    let right = Fixture::open("isolation-right");

    // Same relative paths, same symbol names, different Workspaces.
    assert_ne!(left.resource(DEFINITION).id, right.resource(DEFINITION).id);

    left.save(
        ADMIN,
        "def admin_preview(user_id: str) -> dict[str, str]:\n    return {\"user_id\": user_id}\n",
    );

    assert_eq!(left.caller_files(&left.target()).len(), 2);
    assert_eq!(
        right.caller_files(&right.target()).len(),
        3,
        "a save in one Workspace changed another's graph"
    );
    // Neither index can even name the other's identities.
    assert!(
        right
            .resources()
            .get_by_id(left.resource(DEFINITION).id)
            .expect("lookup")
            .is_none()
    );
}

// =================================================================
// 15. The false-zero matrix, on the representative Workspace.
// =================================================================

#[test]
fn the_false_zero_matrix_holds_through_the_integration_surfaces() {
    let fixture = Fixture::open("false-zero");
    let target = fixture.target();

    // complete + current -> a safe negative.
    let package = fixture.outgoing(&fixture.file(PACKAGE_INIT), &[]);
    assert_eq!(package.confirmed_count(), 0);
    assert_eq!(
        package.answer_state(),
        AnswerState::NoneUnderCompleteCoverage
    );

    // unresolved evidence -> incomplete.
    let annotated = fixture.outgoing(&target, &[]);
    assert!(annotated.coverage.gaps > 0);
    assert_eq!(
        annotated.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );

    // PARTIAL support -> incomplete.
    fixture.write(
        "src/Widget.svelte",
        "<script>\n  export let value = 1\n</script>\n\n<p>{value}</p>\n",
    );
    fixture.ingest(&[RawWatchEvent::Created {
        path: fixture.path("src/Widget.svelte"),
    }]);
    fixture.reconcile();
    let partial = fixture.outgoing(&fixture.file("src/Widget.svelte"), &[]);
    assert_eq!(partial.confirmed_count(), 0);
    assert_eq!(
        partial.coverage.scope.expect("forward scope").support,
        Support::Partial
    );
    assert!(partial.coverage.limits().has(CoverageLimit::PartialSupport));

    // An unsupported language -> incomplete, and never a confirmed zero.
    fixture.write("src/notes.txt", "build_profile is mentioned here\n");
    fixture.ingest(&[RawWatchEvent::Created {
        path: fixture.path("src/notes.txt"),
    }]);
    fixture.reconcile();
    let unsupported = fixture.outgoing(&fixture.file("src/notes.txt"), &[]);
    assert_eq!(unsupported.confirmed_count(), 0);
    assert_eq!(
        unsupported.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );
    assert!(
        unsupported
            .coverage
            .limits()
            .has(CoverageLimit::UnsupportedScope)
    );

    // DIRTY relation component -> incomplete, last-valid still returned.
    fixture.set_relation_state(ADMIN, "DIRTY");
    let dirty = fixture.outgoing(
        &fixture.declaration(ADMIN, "admin_preview"),
        &[RelationKind::Calls],
    );
    assert_eq!(
        dirty.confirmed_count(),
        1,
        "the last valid edge is withheld"
    );
    assert!(
        dirty
            .coverage
            .limits()
            .has(CoverageLimit::DirtyRelationComponent)
    );
    let dirty_zero = fixture.outgoing(
        &fixture.declaration(ADMIN, "admin_preview"),
        &[RelationKind::Extends],
    );
    assert_eq!(
        dirty_zero.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );

    // Traversal truncation -> incomplete.
    let truncated = fixture
        .traversal()
        .run(
            ImpactIntent::PublicSignatureChange,
            &target,
            &Budget {
                max_edges: 0,
                ..generous()
            },
        )
        .expect("impact");
    assert_eq!(truncated.edges.len(), 0);
    assert_eq!(
        truncated.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );

    // A confirmed external relation stays confirmed with no local
    // definition source anywhere in the Workspace.
    let external = fixture.outgoing(&fixture.file(CONFIG), &[RelationKind::Imports]);
    let import = external
        .confirmed
        .iter()
        .find(|relation| matches!(relation.target, GraphEndpoint::External(_)))
        .expect("`import os` is a confirmed external import");
    assert_eq!(import.resolution, Resolution::Resolved);
    assert_eq!(external.answer_state(), AnswerState::Confirmed);
}

// =================================================================
// Duplication and integrity of the final stored graph.
// =================================================================

#[test]
fn the_published_graph_holds_no_duplicate_identity_or_evidence() {
    let fixture = Fixture::open("integrity");

    // Repeated refreshes of the same content must not accumulate.
    let keys = fixture.all_relation_keys();
    let occurrences = fixture.count("SELECT COUNT(*) FROM occurrence");
    let source = fs::read_to_string(fixture.path(SERVICE)).expect("source");
    for _ in 0..3 {
        fixture.save(SERVICE, &format!("{source}\n"));
        fixture.save(SERVICE, &source);
    }
    assert_eq!(
        fixture.all_relation_keys(),
        keys,
        "a refresh duplicated a relation"
    );
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM occurrence"),
        occurrences,
        "a refresh duplicated an Occurrence"
    );

    // Canonical identity, not row count: no two rows share one
    // (kind, source, target).
    let mut unique: BTreeSet<&String> = BTreeSet::new();
    for key in &keys {
        assert!(unique.insert(key), "duplicate canonical relation {key}");
    }

    // One Occurrence proves at most one canonical relation, so a call
    // site cannot be counted as CALLS and again as REFERENCES.
    assert_eq!(
        fixture.count(
            "SELECT COUNT(*) FROM (SELECT occurrence.id FROM occurrence \
             WHERE occurrence.relation_id IS NOT NULL GROUP BY occurrence.id \
             HAVING COUNT(DISTINCT occurrence.relation_id) > 1)"
        ),
        0
    );
    let callers = fixture.incoming(&fixture.target(), &[RelationKind::Calls]);
    let spans: Vec<(String, usize, usize)> = callers
        .confirmed
        .iter()
        .flat_map(|relation| relation.evidence.iter())
        .map(|evidence| {
            (
                fixture.path_of(evidence.resource),
                evidence.span.start_byte,
                evidence.span.end_byte,
            )
        })
        .collect();
    let distinct: BTreeSet<&(String, usize, usize)> = spans.iter().collect();
    assert_eq!(
        spans.len(),
        distinct.len(),
        "an evidence span was counted twice"
    );

    // No reverse rows, and no source body mirror.
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM relation WHERE kind LIKE '%_BY'"),
        0
    );
    let store = fixture.store();
    let mut statement = store
        .connection()
        .prepare("SELECT lookup_name FROM unresolved_reference")
        .expect("statement");
    let names: Vec<String> = statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query")
        .map(|row| row.expect("row"))
        .collect();
    assert!(
        names
            .iter()
            .all(|name| !name.contains('(') && !name.contains('\n')),
        "a source body reached the gap table: {names:?}"
    );
}

/// The whole published graph for the representative Workspace, spelled
/// out. Recall and precision in one assertion: every relation that must
/// exist does, and nothing else was confirmed.
#[test]
fn the_representative_workspace_confirms_exactly_the_true_relations() {
    let fixture = Fixture::open("ground-truth");
    let store = fixture.store();
    let connection = store.connection();
    let mut statement = connection
        .prepare(
            "SELECT relation.kind, \
             COALESCE(source_resource.path_rel, \
                      source_owner.path_rel || '::' || source_symbol.qualified_name) AS source, \
             COALESCE(target_resource.path_rel, \
                      target_owner.path_rel || '::' || target_symbol.qualified_name, \
                      external_entity.package_identity, \
                      domain_entity.kind || ' ' || domain_entity.normalized_identity) AS target \
             FROM relation \
             JOIN graph_entity source_entity ON source_entity.id = relation.source_entity_id \
             LEFT JOIN resource source_resource ON source_resource.id = source_entity.resource_id \
             LEFT JOIN symbol source_symbol ON source_symbol.id = source_entity.symbol_id \
             LEFT JOIN resource source_owner ON source_owner.id = source_symbol.resource_id \
             JOIN graph_entity target_entity ON target_entity.id = relation.target_entity_id \
             LEFT JOIN resource target_resource ON target_resource.id = target_entity.resource_id \
             LEFT JOIN symbol target_symbol ON target_symbol.id = target_entity.symbol_id \
             LEFT JOIN resource target_owner ON target_owner.id = target_symbol.resource_id \
             LEFT JOIN external_entity ON external_entity.id = target_entity.external_entity_id \
             LEFT JOIN domain_entity ON domain_entity.id = target_entity.domain_entity_id \
             ORDER BY 1, 2, 3",
        )
        .expect("statement");
    let confirmed: Vec<String> = statement
        .query_map([], |row| {
            Ok(format!(
                "{} {} -> {}",
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?
            ))
        })
        .expect("query")
        .map(|row| row.expect("row"))
        .collect();

    assert_eq!(
        confirmed,
        [
            "CALLS src/profile_app/admin.py::admin_preview -> src/profile_app/profile.py::build_profile",
            "CALLS src/profile_app/service.py::render_user -> src/profile_app/profile.py::build_profile",
            "CALLS tests/test_profile.py::test_build_profile_uses_requested_locale -> src/profile_app/profile.py::build_profile",
            "IMPORTS src/profile_app/admin.py -> src/profile_app/profile.py",
            "IMPORTS src/profile_app/config.py -> os",
            "IMPORTS src/profile_app/profile.py -> src/profile_app/config.py",
            "IMPORTS src/profile_app/service.py -> src/profile_app/profile.py",
            "IMPORTS tests/test_profile.py -> src/profile_app/profile.py",
            "USES_ENV src/profile_app/config.py::PROFILE_LOCALE -> ENV PROFILE_LOCALE",
        ]
    );

    // Nine relations, nine bound Occurrences: one canonical edge per
    // proving site, and no site proving two edges.
    assert_eq!(fixture.count("SELECT COUNT(*) FROM relation"), 9);
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM occurrence WHERE relation_id IS NOT NULL"),
        9
    );
    // Seven honest gaps, no candidate guessed for any of them.
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM unresolved_reference"),
        7
    );
    assert_eq!(fixture.count("SELECT COUNT(*) FROM relation_candidate"), 0);
}

#[test]
fn repeated_queries_over_an_unchanged_index_are_deterministic() {
    let fixture = Fixture::open("deterministic");
    let target = fixture.target();

    let first = fixture.incoming(&target, &[RelationKind::Calls]);
    let impact_first = fixture
        .traversal()
        .run(ImpactIntent::Rename, &target, &generous())
        .expect("impact");
    let tests_first = fixture
        .tests()
        .for_target(&target, ImpactIntent::PublicSignatureChange, &generous())
        .expect("projection");

    // A fresh set of handles over the same index.db.
    let second = RelationIndex::open(&fixture.db_path())
        .expect("index.db")
        .incoming(&target, &[RelationKind::Calls])
        .expect("query");
    let impact_second = ImpactTraversal::open(&fixture.db_path())
        .expect("index.db")
        .run(ImpactIntent::Rename, &target, &generous())
        .expect("impact");
    let tests_second = RelatedTests::open(&fixture.db_path())
        .expect("index.db")
        .for_target(&target, ImpactIntent::PublicSignatureChange, &generous())
        .expect("projection");

    assert_eq!(first, second);
    assert_eq!(impact_first, impact_second);
    assert_eq!(tests_first, tests_second);
}
