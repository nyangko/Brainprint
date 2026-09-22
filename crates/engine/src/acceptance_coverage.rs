//! Graceful coverage and false-zero prevention (#17 task 14).
//!
//! Tasks 8-13 each produce an answer. This suite is about what those
//! answers are *allowed to claim* when they are empty: every one of
//! them has to be able to tell "nothing exists here" apart from
//! "nothing was found, and here is exactly what stopped the search".
//!
//! These are acceptance tests over the real surfaces -- a scanned
//! Workspace, the real baseline/refresh/reconcile lifecycle, the real
//! query, traversal, projection and preparation paths. The primitive
//! unit tests for each dimension live with their own tasks.

use std::{
    env, fs,
    path::PathBuf,
    process,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use crate::{
    component,
    config::WorkspaceConfig,
    coverage::{AnswerState, CoverageLimit},
    domain::DomainKind,
    evidence::{OccurrenceRef, RelationEvidence, replace_resource_graph},
    gaps::{IntendedRelation, UnresolvedEvidence, UnresolvedReason},
    generation,
    graph::{self, ExternalEntity, GraphEndpoint, GraphStore, Relation, RelationKind},
    impact::{Budget, ImpactIntent, ImpactTraversal},
    prepare::InspectPreparer,
    reconcile::Reconcile,
    refresh::TargetedRefresh,
    related_tests::{ProjectionOutcome, RelatedTests},
    relations::{Direction, RelationAnswer, RelationIndex},
    resolution::{Dispatch, EvidenceBasis, Freshness, Resolution, Support},
    resource::{Resource, ResourceStore},
    scan::BaselineScan,
    structural,
    symbol::{OccurrenceKind, SymbolStore},
    watch::{RawWatchEvent, WatchIngest},
};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------
// A Workspace with nothing unknown in it. Every mention resolves, so
// a zero answer here is allowed to be a real zero.
// ---------------------------------------------------------------

const PURE_TS: &str = "\
export function pure(): number {
  return 1
}
";

const CALC_TS: &str = "\
export function calc(): number {
  return 1
}

export function caller(): number {
  return calc()
}
";

const CALC_TEST_TS: &str = "\
import { calc } from '../src/calc'

export function runs(): number {
  return calc()
}
";

/// A statically named environment key: a confirmed USES_ENV target.
const ENV_STATIC_TS: &str = "\
export function readStatic(): string | undefined {
  return process.env.STATIC_KEY
}
";

// ---------------------------------------------------------------
// A Workspace with the gaps a structural tier legitimately leaves.
// ---------------------------------------------------------------

/// A dynamic key: the API is certain, the key is a runtime value.
const ENV_DYNAMIC_TS: &str = "\
export function readDynamic(name: string): string | undefined {
  return process.env[name]
}
";

const GAP_TS: &str = "\
import { thing } from './missing'

export function gapped(obj: Widget): number {
  obj.compute()
  return 1
}
";

/// An import of a package with no local source at all.
const EXTERNAL_TS: &str = "\
import { readFile } from 'fs'

export function loads(): unknown {
  return readFile
}
";

/// C# config access: one static, one dynamic, and one custom API that
/// this tier deliberately does not recognise.
const SETTINGS_CS: &str = "\
using System.Configuration;

public class Settings
{
    public string Fixed()
    {
        return ConfigurationManager.AppSettings[\"MODE\"];
    }

    public string Dynamic(string name)
    {
        return ConfigurationManager.AppSettings[name];
    }

    public string Custom(string name)
    {
        return MyConfig.Get(\"MODE\");
    }
}
";

/// Container-only structural coverage: PARTIAL support by construction.
const WIDGET_SVELTE: &str = "\
<script>
  export let value = 1
</script>

<p>{value}</p>
";

struct Fixture {
    base: PathBuf,
    root: PathBuf,
}

impl Fixture {
    /// A Workspace whose every mention resolves.
    fn clean(label: &str) -> Self {
        let fixture = Self::empty(label);
        fixture.write("src/pure.ts", PURE_TS);
        fixture.write("src/calc.ts", CALC_TS);
        fixture.write("tests/calc.test.ts", CALC_TEST_TS);
        fixture.write("src/env_static.ts", ENV_STATIC_TS);
        fixture.baseline();
        fixture
    }

    /// A Workspace holding one of every honest gap.
    fn gappy(label: &str) -> Self {
        let fixture = Self::empty(label);
        fixture.write("src/calc.ts", CALC_TS);
        fixture.write("src/gap.ts", GAP_TS);
        fixture.write("src/env_dynamic.ts", ENV_DYNAMIC_TS);
        fixture.write("src/external.ts", EXTERNAL_TS);
        fixture.write("src/Settings.cs", SETTINGS_CS);
        fixture.write("src/Widget.svelte", WIDGET_SVELTE);
        fixture.baseline();
        fixture
    }

    fn empty(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let base = env::temp_dir().join(format!(
            "brainprint-coverage-{label}-{}-{sequence}",
            process::id()
        ));
        let root = base.join("workspace");
        fs::create_dir_all(root.join("src")).expect("src");
        fs::create_dir_all(root.join("tests")).expect("tests");
        Self { base, root }
    }

    fn baseline(&self) {
        BaselineScan::open(&self.db_path())
            .expect("index.db")
            .run_initial_scan(&self.root, &WorkspaceConfig::default(), "workspace-rev-1")
            .expect("baseline scan");
    }

    fn db_path(&self) -> PathBuf {
        self.base.join("data").join("index.db")
    }

    fn write(&self, rel: &str, contents: &str) {
        fs::write(self.root.join(rel), contents).expect("fixture file");
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// One saved file, through the watcher and the targeted refresh.
    fn save(&self, rel: &str, contents: &str) {
        self.write(rel, contents);
        WatchIngest::open(&self.db_path())
            .expect("index.db")
            .ingest_all(
                &self.root,
                &WorkspaceConfig::default(),
                &[RawWatchEvent::Modified {
                    path: self.path(rel),
                }],
            )
            .expect("ingest");
        TargetedRefresh::open(&self.db_path())
            .expect("index.db")
            .run(&self.root, &WorkspaceConfig::default())
            .expect("refresh");
    }

    fn reconcile(&self) {
        Reconcile::open(&self.db_path())
            .expect("index.db")
            .run(&self.root, &WorkspaceConfig::default())
            .expect("reconcile");
    }

    fn resource(&self, rel: &str) -> Resource {
        ResourceStore::open(&self.db_path())
            .expect("index.db")
            .get_active_by_path_key(rel)
            .expect("lookup")
            .expect("the fixture file is a Resource")
    }

    fn file(&self, rel: &str) -> GraphEndpoint {
        GraphEndpoint::Resource(self.resource(rel).id)
    }

    fn declaration(&self, rel: &str, qualified_name: &str) -> GraphEndpoint {
        GraphEndpoint::Symbol(
            SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_for_resource(self.resource(rel).id)
                .expect("symbols")
                .into_iter()
                .find(|symbol| symbol.qualified_name == qualified_name)
                .unwrap_or_else(|| panic!("{qualified_name} is indexed"))
                .id,
        )
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

    /// Set the `RELATION_INDEX` component state one Resource is
    /// published under, the way task 13's own revalidation does.
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
        assert_eq!(updated, 1, "the Resource has a RELATION_INDEX row");
    }

    fn relation_state(&self, rel: &str) -> Option<String> {
        let store = self.store();
        store
            .connection()
            .query_row(
                "SELECT freshness_state FROM component_state \
                 WHERE component_kind = ?1 AND scope_key = ?2",
                rusqlite::params![
                    component::RELATION_INDEX,
                    structural::scope_key(self.resource(rel).id)
                ],
                |row| row.get::<_, String>(0),
            )
            .ok()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

/// A distinct canonical external identity, for candidate sets that
/// have to be longer than task 7 keeps.
fn external(index: usize) -> ExternalEntity {
    ExternalEntity {
        package_identity: format!("pkg-{index}"),
        module_path: None,
        symbol_name: None,
        qualified_name: None,
        kind: "PACKAGE".to_owned(),
        resolved_version: None,
        declaration_locator: None,
    }
}

fn generous() -> Budget {
    Budget {
        max_nodes: 100,
        max_edges: 100,
        max_depth: 10,
        time_limit: Duration::from_secs(30),
    }
}

/// Every gap reason a query returned, as stored labels.
fn reasons(answer: &RelationAnswer) -> Vec<&'static str> {
    let mut found: Vec<&'static str> = answer.gaps.iter().map(|gap| gap.reason.as_str()).collect();
    found.sort_unstable();
    found
}

// =================================================================
// 1-7. The direct-query false-zero table.
// =================================================================

#[test]
fn zero_confirmed_under_complete_coverage_is_a_safe_negative() {
    let fixture = Fixture::clean("safe-negative");
    let pure = fixture.declaration("src/pure.ts", "pure");

    let answer = fixture.outgoing(&pure, &[RelationKind::Calls]);

    assert_eq!(answer.confirmed_count(), 0);
    assert!(
        answer.coverage.limits().is_complete(),
        "{:?}",
        answer.coverage
    );
    assert_eq!(
        answer.answer_state(),
        AnswerState::NoneUnderCompleteCoverage
    );
    assert!(answer.confirmed_zero_is_none());
    assert_eq!(
        answer.coverage.scope.expect("forward scope").support,
        Support::Supported
    );
    assert_eq!(
        answer.coverage.scope.expect("forward scope").freshness,
        Freshness::Fresh
    );
}

#[test]
fn zero_confirmed_with_unresolved_evidence_is_not_a_negative() {
    let fixture = Fixture::gappy("unresolved");
    let gap_file = fixture.file("src/gap.ts");

    // The specifier `./missing` matches no Resource: one import gap,
    // no import relation.
    let answer = fixture.outgoing(&gap_file, &[RelationKind::Imports]);

    assert_eq!(answer.confirmed_count(), 0);
    assert!(answer.coverage.gaps > 0, "{:?}", answer.coverage);
    assert_eq!(
        answer.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );
    assert!(!answer.confirmed_zero_is_none());
    assert!(
        answer
            .coverage
            .limits()
            .has(CoverageLimit::UnresolvedEvidence)
    );
}

#[test]
fn zero_confirmed_with_ambiguous_candidates_is_not_a_negative() {
    let fixture = Fixture::clean("ambiguous");
    let pure = fixture.declaration("src/pure.ts", "pure");
    let calc = fixture.declaration("src/calc.ts", "calc");
    let caller = fixture.declaration("src/calc.ts", "caller");

    // Two canonical candidates in hand, and neither is promoted.
    fixture.publish_gap(
        "src/calc.ts",
        UnresolvedReason::AmbiguousCandidates,
        RelationKind::Calls,
        vec![calc, pure],
    );

    let answer = fixture.outgoing(&caller, &[RelationKind::Calls]);

    assert_eq!(answer.confirmed_count(), 0);
    assert_eq!(answer.coverage.ambiguous, 1);
    assert_eq!(
        answer.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );
    let limits = answer.coverage.limits();
    assert!(limits.has(CoverageLimit::AmbiguousCandidates));
    assert!(!limits.has(CoverageLimit::CandidateTruncated));
}

#[test]
fn zero_confirmed_with_a_semantic_required_gap_is_not_a_negative() {
    let fixture = Fixture::gappy("semantics");
    let gapped = fixture.declaration("src/gap.ts", "gapped");

    // `obj.compute()`: the receiver's type decides, and I4 is not here.
    let answer = fixture.outgoing(&gapped, &[RelationKind::Calls]);

    assert_eq!(answer.confirmed_count(), 0);
    assert!(
        answer.coverage.requires_semantics > 0,
        "{:?}",
        answer.coverage
    );
    assert!(reasons(&answer).contains(&"RECEIVER_TYPE_REQUIRED"));
    assert_eq!(
        answer.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );
    assert!(
        answer
            .coverage
            .limits()
            .has(CoverageLimit::RequiresSemantics)
    );
}

#[test]
fn zero_confirmed_with_an_unsupported_construct_is_not_a_negative() {
    let fixture = Fixture::gappy("unsupported-construct");
    let dynamic = fixture.declaration("src/env_dynamic.ts", "readDynamic");

    let answer = fixture.outgoing(&dynamic, &[RelationKind::UsesEnv]);

    assert_eq!(answer.confirmed_count(), 0);
    assert!(
        answer.coverage.unsupported_construct > 0,
        "{:?}",
        answer.coverage
    );
    assert_eq!(
        answer.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );
    assert!(
        answer
            .coverage
            .limits()
            .has(CoverageLimit::UnsupportedConstruct)
    );
}

#[test]
fn zero_confirmed_under_partial_support_is_not_a_negative() {
    let fixture = Fixture::gappy("partial");
    let widget = fixture.file("src/Widget.svelte");

    // Container-only coverage: relation extraction never ran over the
    // whole file, so zero here says nothing about the file.
    let answer = fixture.outgoing(&widget, &[]);

    assert_eq!(answer.confirmed_count(), 0);
    assert_eq!(
        answer.coverage.scope.expect("forward scope").support,
        Support::Partial
    );
    assert_eq!(
        answer.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );
    assert!(answer.coverage.limits().has(CoverageLimit::PartialSupport));
}

#[test]
fn zero_confirmed_under_a_dirty_relation_index_is_not_a_negative() {
    let fixture = Fixture::clean("dirty");
    let pure = fixture.declaration("src/pure.ts", "pure");
    let owner = fixture.declaration("src/calc.ts", "caller");

    // Before: the same query is a safe negative.
    assert_eq!(
        fixture
            .outgoing(&pure, &[RelationKind::Calls])
            .answer_state(),
        AnswerState::NoneUnderCompleteCoverage
    );

    fixture.set_relation_state("src/calc.ts", "DIRTY");

    // The last valid edges are still returned -- withholding them
    // would be a false zero of its own...
    let confirmed = fixture.outgoing(&owner, &[]);
    assert!(confirmed.confirmed_count() > 0);
    assert_eq!(confirmed.answer_state(), AnswerState::Confirmed);
    // ...but they are not current truth.
    assert_eq!(
        confirmed.coverage.scope.expect("forward scope").freshness,
        Freshness::Dirty
    );
    assert!(
        confirmed
            .coverage
            .limits()
            .has(CoverageLimit::DirtyRelationComponent)
    );

    // And a zero answer from the dirty scope is not a negative.
    let caller = fixture.declaration("src/calc.ts", "caller");
    let zero = fixture.outgoing(&caller, &[RelationKind::Imports]);
    assert_eq!(zero.confirmed_count(), 0);
    assert_eq!(zero.answer_state(), AnswerState::NoneWithIncompleteCoverage);
}

// =================================================================
// 8-10. Stale evidence, truncation, and reverse attribution.
// =================================================================

#[test]
fn stale_evidence_prevents_a_complete_claim() {
    let fixture = Fixture::clean("stale");
    let calc = fixture.declaration("src/calc.ts", "calc");

    let before = fixture
        .traversal()
        .run(ImpactIntent::Rename, &calc, &generous())
        .expect("impact");
    assert!(before.coverage.is_complete(), "{:?}", before.coverage);

    // The Resources moved past the revision the evidence was
    // extracted from.
    fixture
        .store()
        .connection()
        .execute(
            "UPDATE resource SET resource_revision = 'moved' WHERE state = 'ACTIVE'",
            [],
        )
        .expect("bump revisions");

    let after = fixture
        .traversal()
        .run(ImpactIntent::Rename, &calc, &generous())
        .expect("impact");
    assert!(after.coverage.stale_evidence > 0, "{:?}", after.coverage);
    assert!(after.limits().has(CoverageLimit::StaleEvidence));
    assert!(!after.coverage.is_complete());
}

#[test]
fn candidate_truncation_prevents_a_complete_claim() {
    let fixture = Fixture::clean("candidate-truncation");
    let caller = fixture.declaration("src/calc.ts", "caller");

    // More canonical candidates than task 7 keeps.
    let many: Vec<GraphEndpoint> = (0..20)
        .map(|index| GraphEndpoint::External(external(index)))
        .collect();
    fixture.publish_gap(
        "src/calc.ts",
        UnresolvedReason::AmbiguousCandidates,
        RelationKind::Calls,
        many,
    );

    let answer = fixture.outgoing(&caller, &[RelationKind::Calls]);

    assert_eq!(answer.confirmed_count(), 0);
    assert_eq!(answer.coverage.truncated, 1);
    assert!(answer.gaps[0].candidate_truncated);
    let limits = answer.coverage.limits();
    assert!(limits.has(CoverageLimit::CandidateTruncated));
    assert_eq!(
        answer.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );
}

#[test]
fn reverse_unattributed_gaps_prevent_an_unsafe_negative() {
    let fixture = Fixture::gappy("unattributed");
    let calc = fixture.declaration("src/calc.ts", "calc");

    // `obj.compute()` names no target. It is counted against the
    // reverse question and never attached to `calc` by name.
    let answer = fixture.incoming(&calc, &[RelationKind::Calls]);

    assert!(answer.coverage.unattributed > 0, "{:?}", answer.coverage);
    assert!(
        answer.gaps.iter().all(|gap| gap.lookup_name != "compute"),
        "an unresolved row was attached by name"
    );
    let limits = answer.coverage.limits();
    assert!(limits.has(CoverageLimit::UnattributedGaps));
    assert!(limits.has(CoverageLimit::ReverseScopeNotEnumerable));
    assert_ne!(
        answer.answer_state(),
        AnswerState::NoneUnderCompleteCoverage
    );
}

// =================================================================
// 11-12. Traversal completeness.
// =================================================================

#[test]
fn traversal_truncation_prevents_a_complete_impact_claim() {
    let fixture = Fixture::clean("traversal-truncation");
    let calc = fixture.declaration("src/calc.ts", "calc");

    let tight = Budget {
        max_edges: 0,
        ..generous()
    };
    let result = fixture
        .traversal()
        .run(ImpactIntent::Rename, &calc, &tight)
        .expect("impact");

    assert_eq!(result.edges.len(), 0);
    assert!(result.truncation.is_some());
    assert!(result.limits().has(CoverageLimit::TraversalTruncated));
    assert_eq!(
        result.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );
}

#[test]
fn a_complete_traversal_with_no_edges_is_a_safe_negative() {
    let fixture = Fixture::clean("traversal-complete");
    let pure = fixture.declaration("src/pure.ts", "pure");

    let result = fixture
        .traversal()
        .run(ImpactIntent::Rename, &pure, &generous())
        .expect("impact");

    assert_eq!(result.edges.len(), 0);
    assert!(result.truncation.is_none());
    assert!(result.coverage.is_complete(), "{:?}", result.coverage);
    assert_eq!(
        result.answer_state(),
        AnswerState::NoneUnderCompleteCoverage
    );
}

// =================================================================
// 13-15. Related tests.
// =================================================================

#[test]
fn related_test_zero_under_complete_coverage_is_a_safe_negative() {
    let fixture = Fixture::clean("tests-complete");
    let pure = fixture.declaration("src/pure.ts", "pure");

    let projection = fixture
        .tests()
        .for_target(&pure, ImpactIntent::PublicSignatureChange, &generous())
        .expect("projection");

    assert!(projection.candidates.is_empty());
    assert_eq!(
        projection.outcome(),
        ProjectionOutcome::NoneUnderCompleteCoverage
    );
    assert_eq!(
        projection.answer_state(),
        AnswerState::NoneUnderCompleteCoverage
    );
}

#[test]
fn related_test_zero_with_an_unknown_role_is_incomplete() {
    let fixture = Fixture::clean("tests-unknown-role");
    let calc = fixture.declaration("src/calc.ts", "calc");

    // The Resource model no longer classifies the reachable file, so
    // a test among those endpoints would not be recognised.
    fixture
        .store()
        .connection()
        .execute("UPDATE resource SET role = 'UNKNOWN'", [])
        .expect("role");

    let projection = fixture
        .tests()
        .for_target(&calc, ImpactIntent::PublicSignatureChange, &generous())
        .expect("projection");

    assert!(projection.candidates.is_empty());
    assert!(projection.coverage.unknown_role > 0);
    assert!(
        projection
            .coverage
            .limits()
            .has(CoverageLimit::UnknownResourceRole)
    );
    assert_eq!(
        projection.outcome(),
        ProjectionOutcome::NoneWithIncompleteCoverage
    );
}

#[test]
fn related_test_zero_with_traversal_truncation_is_incomplete() {
    let fixture = Fixture::clean("tests-truncated");
    let calc = fixture.declaration("src/calc.ts", "calc");

    let tight = Budget {
        max_edges: 0,
        ..generous()
    };
    let projection = fixture
        .tests()
        .for_target(&calc, ImpactIntent::PublicSignatureChange, &tight)
        .expect("projection");

    assert!(projection.candidates.is_empty());
    assert!(projection.coverage.traversal_truncation.is_some());
    assert!(
        projection
            .coverage
            .limits()
            .has(CoverageLimit::TraversalTruncated)
    );
    assert_eq!(
        projection.outcome(),
        ProjectionOutcome::NoneWithIncompleteCoverage
    );
}

// =================================================================
// 16-18. External targets and prepared source.
// =================================================================

#[test]
fn a_confirmed_external_import_stays_confirmed_without_local_source() {
    let fixture = Fixture::gappy("external");
    let external_file = fixture.file("src/external.ts");

    let answer = fixture.outgoing(&external_file, &[RelationKind::Imports]);
    let import = answer
        .confirmed
        .iter()
        .find(|relation| matches!(relation.target, GraphEndpoint::External(_)))
        .expect("the external package is a confirmed target");

    assert_eq!(import.resolution, Resolution::Resolved);
    assert!(!import.evidence.is_empty());
    assert_eq!(answer.answer_state(), AnswerState::Confirmed);
    // Its absence of local source is not a gap.
    assert!(
        answer.gaps.iter().all(|gap| gap.lookup_name != "fs"),
        "an external package was reported as unresolved"
    );
}

#[test]
fn preparation_fabricates_no_source_for_an_external_target() {
    let fixture = Fixture::gappy("external-prepare");
    let external_file = fixture.file("src/external.ts");

    let answer = fixture.outgoing(&external_file, &[RelationKind::Imports]);
    let external = answer
        .confirmed
        .iter()
        .find(|relation| matches!(relation.target, GraphEndpoint::External(_)))
        .expect("external target")
        .target
        .clone();

    let prepared = fixture
        .preparer()
        .prepare(&external, Direction::Incoming, &[RelationKind::Imports])
        .expect("prepare");

    assert!(
        prepared.target.is_none(),
        "an external package has no definition source to prepare"
    );
    // The relation itself is still there, with its evidence read from
    // the importing file.
    assert_eq!(prepared.confirmed_count(), 1);
}

#[test]
fn a_failed_source_read_degrades_preparation_without_deleting_the_relation() {
    let fixture = Fixture::clean("prepare-degraded");
    let calc = fixture.declaration("src/calc.ts", "calc");

    let before = fixture
        .preparer()
        .prepare(&calc, Direction::Incoming, &[RelationKind::Calls])
        .expect("prepare");
    assert!(before.confirmed_count() > 0);
    assert!(before.source_complete());

    // The evidence file is gone; nothing may be sliced from it.
    fs::remove_file(fixture.path("tests/calc.test.ts")).expect("remove");

    let after = fixture
        .preparer()
        .prepare(&calc, Direction::Incoming, &[RelationKind::Calls])
        .expect("prepare");

    assert_eq!(
        after.confirmed_count(),
        before.confirmed_count(),
        "a failed read must not delete a confirmed relation"
    );
    assert!(!after.source_complete());
    assert!(
        after
            .relations
            .iter()
            .flat_map(|relation| &relation.evidence)
            .any(|evidence| evidence.unavailable.is_some())
    );
}

#[test]
fn source_availability_does_not_make_an_incomplete_graph_look_complete() {
    let fixture = Fixture::gappy("prepare-separation");
    let gapped = fixture.declaration("src/gap.ts", "gapped");

    let prepared = fixture
        .preparer()
        .prepare(&gapped, Direction::Outgoing, &[RelationKind::Calls])
        .expect("prepare");

    // Every byte that was asked for was read...
    assert!(prepared.source_complete());
    // ...and the graph behind it is still incomplete.
    assert!(!prepared.coverage.limits().is_complete());
    assert_eq!(
        prepared.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );
}

// =================================================================
// 19-23. Domain relations: dynamic keys and dispatch.
// =================================================================

#[test]
fn a_dynamic_env_key_creates_no_domain_entity() {
    let fixture = Fixture::gappy("dynamic-entity");
    let store = fixture.store();

    let keys: Vec<String> = store
        .connection()
        .prepare("SELECT normalized_identity FROM domain_entity WHERE kind = 'ENV'")
        .expect("statement")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query")
        .map(|row| row.expect("row"))
        .collect();

    assert!(
        !keys
            .iter()
            .any(|key| key.contains("unknown") || key == "name"),
        "a dynamic key was turned into an entity: {keys:?}"
    );
}

#[test]
fn a_dynamic_env_key_prevents_a_falsely_complete_zero() {
    let fixture = Fixture::gappy("dynamic-env");
    let dynamic = fixture.declaration("src/env_dynamic.ts", "readDynamic");

    let answer = fixture.outgoing(&dynamic, &[RelationKind::UsesEnv]);

    assert_eq!(answer.confirmed_count(), 0);
    assert_eq!(reasons(&answer), ["DYNAMIC_KEY_EXPRESSION"]);
    assert_eq!(answer.gaps[0].lookup_name, "process.env");
    assert!(answer.gaps[0].candidates.is_empty());
    assert_eq!(
        answer.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );
}

#[test]
fn a_dynamic_config_key_follows_the_same_rule() {
    let fixture = Fixture::gappy("dynamic-config");
    let dynamic = fixture.declaration("src/Settings.cs", "Settings.Dynamic");

    let answer = fixture.outgoing(&dynamic, &[RelationKind::UsesConfig]);

    assert_eq!(answer.confirmed_count(), 0);
    assert_eq!(reasons(&answer), ["DYNAMIC_KEY_EXPRESSION"]);
    assert_eq!(
        answer.gaps[0].intended,
        IntendedRelation::Known(DomainKind::Config.relation_kind())
    );
    assert_eq!(
        answer.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );

    // The statically named sibling is a confirmed relation.
    let fixed = fixture.declaration("src/Settings.cs", "Settings.Fixed");
    let confirmed = fixture.outgoing(&fixed, &[RelationKind::UsesConfig]);
    assert_eq!(confirmed.confirmed_count(), 1);
    assert_eq!(confirmed.answer_state(), AnswerState::Confirmed);
}

#[test]
fn an_unrecognised_config_api_fabricates_nothing() {
    let fixture = Fixture::gappy("unrecognised-config");
    let custom = fixture.declaration("src/Settings.cs", "Settings.Custom");

    let answer = fixture.outgoing(&custom, &[RelationKind::UsesConfig]);

    // `MyConfig.Get("MODE")` is not a recognised config API. It is not
    // a confirmed relation, and it is not invented as a gap either --
    // this tier saw a method call, which is all it saw.
    assert_eq!(answer.confirmed_count(), 0);
    assert!(answer.gaps.is_empty(), "{:?}", answer.gaps);
}

#[test]
fn a_structurally_confirmed_dynamic_dispatch_relation_stays_confirmed() {
    let fixture = Fixture::clean("dynamic-dispatch");
    let caller = fixture.declaration("src/calc.ts", "caller");

    // A confirmed edge that legitimately carries DYNAMIC dispatch.
    // A new canonical identity, so the dispatch on it is this
    // publication's and not the baseline's.
    let pure = fixture.declaration("src/pure.ts", "pure");
    fixture.publish_dynamic_edge("src/calc.ts", &caller, &pure);

    let answer = fixture.outgoing(&caller, &[RelationKind::Calls]);
    let relation = &answer.confirmed[0];

    assert_eq!(relation.dispatch, Dispatch::Dynamic);
    assert_eq!(relation.resolution, Resolution::Resolved);
    assert_eq!(answer.answer_state(), AnswerState::Confirmed);
    // Being dynamic is not by itself a coverage limit.
    assert!(answer.coverage.limits().is_complete());
}

// =================================================================
// 24-27. Lifecycle.
// =================================================================

#[test]
fn a_building_relation_publication_stays_invisible() {
    let fixture = Fixture::clean("building");
    let pure = fixture.declaration("src/pure.ts", "pure");
    let caller = fixture.declaration("src/calc.ts", "caller");

    let before = fixture
        .outgoing(&caller, &[RelationKind::Calls])
        .confirmed_count();

    let store = fixture.store();
    let connection = store.connection();
    let revision = generation::current_workspace_revision(connection)
        .expect("clock")
        .expect("bootstrapped");
    let building = generation::begin_generation(connection, &revision).expect("begin");
    let transaction = connection.unchecked_transaction().expect("transaction");
    let (_record, grant) = generation::grant_publication(&transaction, building.id).expect("grant");
    for endpoint in [&caller, &pure] {
        graph::ensure_entity(&transaction, endpoint).expect("ensure");
    }
    replace_resource_graph(
        &transaction,
        &grant,
        &fixture.basis("src/calc.ts", building.id),
        &[RelationEvidence {
            occurrence: fixture.call_site("src/calc.ts"),
            relation: Relation {
                kind: RelationKind::Calls,
                source: caller.clone(),
                target: pure,
                dispatch: Dispatch::Static,
                created_generation: building.id,
            },
        }],
        &[],
    )
    .expect("replace");
    transaction.commit().expect("commit");
    // No `finish_publish_stable`: the generation is still BUILDING.

    assert_eq!(
        fixture
            .outgoing(&caller, &[RelationKind::Calls])
            .confirmed_count(),
        before,
        "a BUILDING publication became queryable"
    );
}

#[test]
fn a_last_valid_relation_under_a_dirty_lifecycle_is_not_current() {
    let fixture = Fixture::clean("last-valid");
    let caller = fixture.declaration("src/calc.ts", "caller");

    fixture.set_relation_state("src/calc.ts", "DIRTY");

    let answer = fixture.outgoing(&caller, &[]);

    // Still returned, and still confirmed...
    assert!(answer.confirmed_count() > 0);
    // ...and every one of them is marked not-current rather than
    // presented as current truth.
    assert!(
        answer
            .confirmed
            .iter()
            .all(|relation| relation.freshness == Freshness::Dirty)
    );
    assert!(
        answer
            .coverage
            .limits()
            .has(CoverageLimit::DirtyRelationComponent)
    );
}

#[test]
fn completeness_recovers_after_revalidation_returns_current() {
    let fixture = Fixture::clean("recovery");

    fixture.set_relation_state("src/calc.ts", "DIRTY");
    let caller = fixture.declaration("src/calc.ts", "caller");
    assert_eq!(
        fixture
            .outgoing(&caller, &[RelationKind::Imports])
            .answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );

    // A real save, through the watcher and task 13's targeted refresh.
    fixture.save(
        "src/calc.ts",
        "\
export function calc(): number {
  return 2
}

export function caller(): number {
  return calc()
}
",
    );

    assert_eq!(
        fixture.relation_state("src/calc.ts").as_deref(),
        Some("CURRENT")
    );
    let caller = fixture.declaration("src/calc.ts", "caller");
    let answer = fixture.outgoing(&caller, &[RelationKind::Imports]);
    assert_eq!(
        answer.answer_state(),
        AnswerState::NoneUnderCompleteCoverage
    );
    // And the confirmed edge came back current too.
    assert!(
        fixture
            .outgoing(&caller, &[RelationKind::Calls])
            .confirmed
            .iter()
            .all(|relation| relation.freshness == Freshness::Fresh)
    );
}

#[test]
fn a_deleted_target_cannot_produce_a_false_complete_zero() {
    let fixture = Fixture::clean("delete");
    let test_file = fixture.file("tests/calc.test.ts");

    assert!(
        fixture
            .outgoing(&test_file, &[RelationKind::Imports])
            .confirmed_count()
            > 0
    );

    fs::remove_file(fixture.path("src/calc.ts")).expect("remove");
    fixture.reconcile();

    let answer = fixture.outgoing(&test_file, &[RelationKind::Imports]);

    assert_eq!(
        answer.confirmed_count(),
        0,
        "the edge to a deleted target is gone"
    );
    assert!(
        !answer.coverage.limits().is_complete(),
        "a deleted target left a complete-looking zero: {:?}",
        answer.coverage
    );
    assert_eq!(
        answer.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );
}

// =================================================================
// 28-31. Cross-cutting invariants.
// =================================================================

#[test]
fn the_three_truncation_dimensions_stay_distinguishable() {
    let fixture = Fixture::clean("truncation-dimensions");
    let calc = fixture.declaration("src/calc.ts", "calc");
    let caller = fixture.declaration("src/calc.ts", "caller");

    fixture.publish_gap(
        "src/calc.ts",
        UnresolvedReason::AmbiguousCandidates,
        RelationKind::Calls,
        (0..20)
            .map(|index| GraphEndpoint::External(external(index)))
            .collect(),
    );

    // Candidate truncation, on its own.
    let candidates = fixture.outgoing(&caller, &[RelationKind::Calls]);
    let candidate_limits = candidates.coverage.limits();
    assert!(candidate_limits.has(CoverageLimit::CandidateTruncated));
    assert!(!candidate_limits.has(CoverageLimit::TraversalTruncated));

    // Traversal truncation, on its own.
    let walk = fixture
        .traversal()
        .run(
            ImpactIntent::Rename,
            &calc,
            &Budget {
                max_edges: 0,
                ..generous()
            },
        )
        .expect("impact");
    let walk_limits = walk.limits();
    assert!(walk_limits.has(CoverageLimit::TraversalTruncated));
    assert!(!walk_limits.has(CoverageLimit::CandidateTruncated));
    assert!(!walk_limits.has(CoverageLimit::SupportingPathTruncated));

    // And the supporting-path cut is a third label again.
    assert_ne!(
        CoverageLimit::SupportingPathTruncated,
        CoverageLimit::CandidateTruncated
    );
}

#[test]
fn repeated_queries_produce_identical_coverage_and_outcome() {
    let fixture = Fixture::gappy("deterministic");
    let gapped = fixture.declaration("src/gap.ts", "gapped");

    let first = fixture.outgoing(&gapped, &[]);
    let second = fixture.outgoing(&gapped, &[]);
    let third = RelationIndex::open(&fixture.db_path())
        .expect("index.db")
        .outgoing(&gapped, &[])
        .expect("query");

    assert_eq!(first.coverage, second.coverage);
    assert_eq!(first.coverage, third.coverage);
    assert_eq!(first.coverage.limits(), third.coverage.limits());
    assert_eq!(first.answer_state(), third.answer_state());
    assert_eq!(first.gaps, third.gaps);
}

#[test]
fn an_incomplete_answer_recovers_nothing_by_text_or_name() {
    let fixture = Fixture::gappy("no-fallback");
    let gapped = fixture.declaration("src/gap.ts", "gapped");
    // A same-named declaration elsewhere in the Workspace: exactly
    // what a name-similarity fallback would have reached for.
    let bait = fixture.declaration("src/Settings.cs", "Settings.Dynamic");

    let answer = fixture.outgoing(&gapped, &[]);
    let impact = fixture
        .traversal()
        .run(ImpactIntent::Rename, &gapped, &generous())
        .expect("impact");

    // The gaps stay gaps: the coverage is reported as incomplete
    // rather than filled in.
    assert!(!answer.coverage.limits().is_complete());
    assert!(!impact.limits().is_complete());
    assert!(
        answer
            .confirmed
            .iter()
            .all(|relation| relation.target != bait),
        "an unresolved name was resolved to a same-named declaration"
    );
    // Nor did any gap quietly acquire a candidate it was never given.
    assert!(
        answer
            .gaps
            .iter()
            .filter(|gap| gap.reason != UnresolvedReason::AmbiguousCandidates)
            .all(|gap| gap.candidates.is_empty()),
        "a candidate appeared for a gap the resolver had none for"
    );
}

#[test]
fn no_source_body_is_mirrored_into_the_index() {
    let fixture = Fixture::gappy("no-mirror");
    let store = fixture.store();

    // Bodies, never identities: a stored `lookup_name` is a name, and
    // `process.env` is an API path, not source text.
    for needle in ["return process.env[name]", "obj.compute()", "return 1"] {
        assert!(
            !text_of_index(store.connection()).contains(needle),
            "source body `{needle}` reached index.db"
        );
    }
}

/// Every text value stored anywhere a relation answer reads from.
fn text_of_index(connection: &rusqlite::Connection) -> String {
    let mut collected = String::new();
    for sql in [
        "SELECT lookup_name || ' ' || COALESCE(module_hint, '') FROM unresolved_reference",
        "SELECT package_identity || ' ' || COALESCE(module_path, '') FROM external_entity",
        "SELECT display_label || ' ' || normalized_identity FROM domain_entity",
    ] {
        let mut statement = connection.prepare(sql).expect("statement");
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query");
        for row in rows {
            collected.push_str(&row.expect("row"));
            collected.push('\n');
        }
    }
    collected
}

// ---------------------------------------------------------------
// Fixture helpers that publish hand-built evidence, for the two
// dimensions no fixture source can produce deterministically.
// ---------------------------------------------------------------

impl Fixture {
    fn basis(&self, rel: &str, generation_id: i64) -> EvidenceBasis {
        let resource = self.resource(rel);
        let profile_id = SymbolStore::open(&self.db_path())
            .expect("index.db")
            .list_for_resource(resource.id)
            .expect("symbols")
            .first()
            .expect("the file declares something")
            .analysis_profile_id;
        EvidenceBasis {
            owner_resource: resource.id,
            owner_resource_revision: resource.resource_revision,
            generation_id,
            analysis_profile_id: profile_id,
            resolution_context_key: None,
        }
    }

    /// The first call site in a file, as an Occurrence anchor.
    fn call_site(&self, rel: &str) -> OccurrenceRef {
        self.anchor(rel, OccurrenceKind::CallSite)
    }

    fn anchor(&self, rel: &str, kind: OccurrenceKind) -> OccurrenceRef {
        SymbolStore::open(&self.db_path())
            .expect("index.db")
            .list_occurrences_for_resource(self.resource(rel).id)
            .expect("occurrences")
            .into_iter()
            .find(|occurrence| occurrence.kind == kind)
            .map(|occurrence| OccurrenceRef {
                kind: occurrence.kind,
                start_byte: occurrence.span.start_byte,
                end_byte: occurrence.span.end_byte,
            })
            .expect("the file has such a site")
    }

    /// Publish one hand-built gap against a file's first call site.
    fn publish_gap(
        &self,
        rel: &str,
        reason: UnresolvedReason,
        intended: RelationKind,
        candidates: Vec<GraphEndpoint>,
    ) {
        self.publish(
            rel,
            Vec::new(),
            vec![UnresolvedEvidence {
                occurrence: self.call_site(rel),
                intended: IntendedRelation::Known(intended),
                lookup_name: "target".to_owned(),
                module_hint: None,
                reason,
                candidates,
            }],
        );
    }

    /// Publish one confirmed edge that carries DYNAMIC dispatch.
    fn publish_dynamic_edge(&self, rel: &str, source: &GraphEndpoint, target: &GraphEndpoint) {
        let published = generation::GenerationStore::open(&self.db_path())
            .expect("index.db")
            .current_stable()
            .expect("generation")
            .expect("published")
            .id;
        self.publish(
            rel,
            vec![RelationEvidence {
                occurrence: self.call_site(rel),
                relation: Relation {
                    kind: RelationKind::Calls,
                    source: source.clone(),
                    target: target.clone(),
                    dispatch: Dispatch::Dynamic,
                    created_generation: published,
                },
            }],
            Vec::new(),
        );
    }

    fn publish(
        &self,
        rel: &str,
        resolved: Vec<RelationEvidence>,
        unresolved: Vec<UnresolvedEvidence>,
    ) {
        let store = self.store();
        let connection = store.connection();
        let revision = generation::current_workspace_revision(connection)
            .expect("clock")
            .expect("bootstrapped");
        let building = generation::begin_generation(connection, &revision).expect("begin");
        let transaction = connection.unchecked_transaction().expect("transaction");
        let (record, grant) =
            generation::grant_publication(&transaction, building.id).expect("grant");
        for item in &resolved {
            for endpoint in [&item.relation.source, &item.relation.target] {
                graph::ensure_entity(&transaction, endpoint).expect("ensure");
            }
        }
        for gap in &unresolved {
            for candidate in &gap.candidates {
                graph::ensure_entity(&transaction, candidate).expect("ensure");
            }
        }
        replace_resource_graph(
            &transaction,
            &grant,
            &self.basis(rel, building.id),
            &resolved,
            &unresolved,
        )
        .expect("replace");
        generation::finish_publish_stable(&transaction, &record).expect("stable");
        transaction.commit().expect("commit");
    }
}
