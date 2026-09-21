//! Related-test candidates, projected from confirmed relations (#17
//! task 11).
//!
//! "Which tests cover this?" is answered here without adding anything to
//! the graph. There is no `RELATED_TEST`, `TESTED_BY`, or `TESTS`
//! relation kind, and nothing this module produces is persisted: a
//! related test is a *view* of facts tasks 4-10 already established --
//! a Resource the Resource model classified `TEST`, and a confirmed
//! relation path from it to what changed.
//!
//! ## What may make a candidate
//!
//! Both halves are required, and neither is negotiable:
//!
//! 1. the candidate's Resource is [`ResourceRole::Test`]. The Resource
//!    model is the only test authority (#16); this module never decides
//!    that a file is a test, and in particular never from its name.
//! 2. a **confirmed** relation path reaches the target. Unresolved
//!    evidence and candidate targets stay gaps; they never become test
//!    linkage.
//!
//! There is deliberately no filename fallback. `FooTest` is not evidence
//! about `Foo`, and this module will return nothing rather than guess --
//! while saying that its coverage was incomplete, so the nothing is not
//! read as "no tests cover this".
//!
//! ## Where the paths come from
//!
//! Task 10's traversal. [`RelatedTests::from_impact`] projects an
//! [`ImpactResult`] that has already been computed -- it re-walks
//! nothing and reads no relation rows, only the Resource roles behind
//! the endpoints the traversal already found.
//! [`RelatedTests::for_target`] is the convenience that runs the
//! traversal first.
//!
//! ## Ranking
//!
//! Structural, never scored: a shorter confirmed path beats a longer
//! one, and canonical identity breaks the tie. No confidence number, no
//! model, nothing opaque.
//!
//! Out of scope: env/config relations (task 12), lifecycle (task 13),
//! test *command* discovery and execution (I6), and any framework's
//! own test semantics.

use std::{collections::HashMap, error::Error, fmt, path::Path};

use brainprint_core::ResourceId;
use rusqlite::{OptionalExtension, params};

use crate::{
    graph::{self, GraphEndpoint, RelationKind},
    impact::{
        Budget, ImpactEdge, ImpactError, ImpactIntent, ImpactResult, ImpactTraversal, Truncation,
    },
    relations::RelationResult,
    resolution::{Freshness, Support, weaker_freshness, weaker_support},
    resource::{ResourceError, ResourceRole},
};

/// How many supporting paths one candidate keeps.
///
/// A bound, and being cut is reported ([`RelatedTestCandidate::
/// paths_truncated`]) rather than silently shortening the explanation.
pub const MAX_SUPPORTING_PATHS: usize = 4;

/// One confirmed relation chain from a test towards the target.
///
/// Hops are ordered test-first: `hops[0]` leaves the test endpoint, and
/// the last hop arrives at the traversal root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestPath {
    pub hops: Vec<RelationResult>,
}

impl TestPath {
    #[must_use]
    pub fn length(&self) -> usize {
        self.hops.len()
    }
}

/// Why a candidate was projected. Structural and deterministic -- never
/// a score, and never a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionBasis {
    /// One confirmed relation from the test straight to the target.
    DirectRelation(RelationKind),
    /// A confirmed chain, through this kind first.
    RelationPath { hops: usize, first: RelationKind },
}

impl ProjectionBasis {
    /// Whether the test reaches the target in one confirmed hop.
    #[must_use]
    pub const fn is_direct(self) -> bool {
        matches!(self, Self::DirectRelation(_))
    }
}

/// One test Resource with confirmed structural reason to be related.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelatedTestCandidate {
    /// The TEST Resource, by stable identity.
    pub resource: ResourceId,
    pub path_rel: String,
    /// The endpoints inside it that reach the target -- the Symbols
    /// when the structure names them, the Resource itself otherwise.
    pub endpoints: Vec<GraphEndpoint>,
    /// Hops along the shortest confirmed path.
    pub distance: usize,
    pub basis: ProjectionBasis,
    /// Confirmed paths kept as the explanation, shortest first.
    pub paths: Vec<TestPath>,
    /// Whether more confirmed paths existed than were kept.
    pub paths_truncated: bool,
    /// The weakest support across the kept evidence.
    pub support: Support,
    /// The weakest freshness across the kept evidence.
    pub freshness: Freshness,
}

/// What the projection could not see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestCoverage {
    /// Task 10's coverage, carried verbatim: gaps, candidates,
    /// semantic-required, unsupported, degraded evidence, and whether
    /// the walk itself was cut.
    pub impact: crate::impact::ImpactCoverage,
    /// Which traversal budget stopped the walk, if one did.
    pub traversal_truncation: Option<Truncation>,
    /// Endpoints owned by a Resource the Resource model classifies
    /// `UNKNOWN`: role coverage is partial there, so a test among them
    /// would not be recognised.
    pub unknown_role: usize,
    /// Endpoints whose owning Resource could not be read at all.
    pub unresolved_owner: usize,
    /// Candidates whose supporting-path list was cut.
    pub paths_truncated: usize,
}

impl TestCoverage {
    /// Whether "these are the related tests" may be claimed outright.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.impact.is_complete()
            && self.traversal_truncation.is_none()
            && self.unknown_role == 0
            && self.unresolved_owner == 0
    }
}

/// What an empty candidate list means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionOutcome {
    /// Confirmed candidates were found.
    Candidates,
    /// None, and the graph coverage behind that was complete: there
    /// really is no confirmed test relation.
    NoneUnderCompleteCoverage,
    /// None *found*, but coverage was incomplete -- a gap, a budget, or
    /// partial role coverage. Not a statement that none exist.
    NoneWithIncompleteCoverage,
}

/// Related tests for one target, with the evidence that justifies them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelatedTestProjection {
    pub target: GraphEndpoint,
    pub intent: ImpactIntent,
    /// Deterministically ordered: shortest confirmed path first, then
    /// canonical identity.
    pub candidates: Vec<RelatedTestCandidate>,
    pub coverage: TestCoverage,
}

impl RelatedTestProjection {
    #[must_use]
    pub fn outcome(&self) -> ProjectionOutcome {
        if !self.candidates.is_empty() {
            return ProjectionOutcome::Candidates;
        }
        if self.coverage.is_complete() {
            ProjectionOutcome::NoneUnderCompleteCoverage
        } else {
            ProjectionOutcome::NoneWithIncompleteCoverage
        }
    }
}

/// Failure projecting related tests.
#[derive(Debug)]
pub enum RelatedTestError {
    Impact(ImpactError),
    Sqlite(rusqlite::Error),
    Resource(ResourceError),
}

impl fmt::Display for RelatedTestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Impact(error) => write!(formatter, "impact traversal: {error}"),
            Self::Sqlite(error) => write!(formatter, "index.db: {error}"),
            Self::Resource(error) => write!(formatter, "resource: {error}"),
        }
    }
}

impl Error for RelatedTestError {}

impl From<ImpactError> for RelatedTestError {
    fn from(error: ImpactError) -> Self {
        Self::Impact(error)
    }
}

impl From<rusqlite::Error> for RelatedTestError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<ResourceError> for RelatedTestError {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

/// Related-test projection over one Workspace's graph.
pub struct RelatedTests {
    traversal: ImpactTraversal,
}

impl RelatedTests {
    /// Open the `index.db` at `path`.
    pub fn open(path: &Path) -> Result<Self, RelatedTestError> {
        Ok(Self::new(ImpactTraversal::open(path)?))
    }

    /// Project over an already-opened traversal.
    #[must_use]
    pub const fn new(traversal: ImpactTraversal) -> Self {
        Self { traversal }
    }

    /// The traversal these projections are built on.
    #[must_use]
    pub const fn traversal(&self) -> &ImpactTraversal {
        &self.traversal
    }

    /// Run the impact traversal for `target` and project its tests.
    pub fn for_target(
        &self,
        target: &GraphEndpoint,
        intent: ImpactIntent,
        budget: &Budget,
    ) -> Result<RelatedTestProjection, RelatedTestError> {
        let impact = self.traversal.run(intent, target, budget)?;
        self.from_impact(&impact)
    }

    /// Project the tests already present in a traversal result.
    ///
    /// No relation row is read and nothing is walked again: the paths
    /// are the ones task 10 confirmed, and the only lookup is each
    /// endpoint's owning Resource and its role.
    pub fn from_impact(
        &self,
        impact: &ImpactResult,
    ) -> Result<RelatedTestProjection, RelatedTestError> {
        let index = PathIndex::of(impact);
        let mut owners = OwnerCache::default();
        let mut coverage = TestCoverage {
            impact: impact.coverage,
            traversal_truncation: impact.truncation,
            unknown_role: 0,
            unresolved_owner: 0,
            paths_truncated: 0,
        };

        // One entry per TEST Resource, however many endpoints or paths
        // inside it reach the target.
        let mut found: Vec<PartialCandidate> = Vec::new();
        for node in &impact.nodes {
            // Depth 0 is the target itself, not something testing it.
            if node.depth == 0 {
                continue;
            }
            let Some(owner) = self.owner_of(&node.endpoint, &mut owners, &mut coverage)? else {
                continue;
            };
            if owner.role != ResourceRole::Test {
                continue;
            }
            let paths = index.paths_from(&node.endpoint, node.depth);
            if paths.is_empty() {
                // Reached, but the edges behind it were not emitted
                // (a budget cut them). Nothing confirmed to show.
                continue;
            }
            match found
                .iter_mut()
                .find(|candidate| candidate.resource == owner.id)
            {
                Some(candidate) => {
                    candidate.endpoints.push(node.endpoint.clone());
                    candidate.paths.extend(paths);
                }
                None => found.push(PartialCandidate {
                    resource: owner.id,
                    path_rel: owner.path_rel.clone(),
                    endpoints: vec![node.endpoint.clone()],
                    paths,
                }),
            }
        }

        let mut candidates: Vec<RelatedTestCandidate> = found
            .into_iter()
            .map(|partial| partial.finish(&mut coverage))
            .collect();
        // Shortest confirmed path first; canonical identity decides the
        // rest. Nothing here depends on a row id.
        candidates.sort_by(|left, right| {
            (
                left.distance,
                graph::endpoint_sort_key(&GraphEndpoint::Resource(left.resource)),
            )
                .cmp(&(
                    right.distance,
                    graph::endpoint_sort_key(&GraphEndpoint::Resource(right.resource)),
                ))
        });

        Ok(RelatedTestProjection {
            target: impact.root.clone(),
            intent: impact.intent,
            candidates,
            coverage,
        })
    }

    /// The Resource behind an endpoint, and its role.
    ///
    /// An external package or domain entity owns no Workspace Resource
    /// and is simply not a test -- that is an answer, not a coverage
    /// limitation. A Resource that cannot be read, or whose role the
    /// model left `UNKNOWN`, is one, and is counted.
    fn owner_of(
        &self,
        endpoint: &GraphEndpoint,
        owners: &mut OwnerCache,
        coverage: &mut TestCoverage,
    ) -> Result<Option<Owner>, RelatedTestError> {
        let resource = match endpoint {
            GraphEndpoint::Resource(id) => *id,
            GraphEndpoint::Symbol(id) => {
                let uid: Option<Vec<u8>> = self
                    .traversal
                    .relations()
                    .connection()
                    .query_row(
                        "SELECT resource.uid FROM symbol \
                         JOIN resource ON resource.id = symbol.resource_id \
                         WHERE symbol.uid = ?1 AND resource.state = 'ACTIVE'",
                        params![id.to_bytes().to_vec()],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(uid) = uid else {
                    coverage.unresolved_owner += 1;
                    return Ok(None);
                };
                ResourceId::from_bytes(sixteen(&uid))
            }
            GraphEndpoint::External(_) | GraphEndpoint::Domain(_) => return Ok(None),
        };

        if let Some(owner) = owners.get(&resource) {
            return Ok(owner.clone());
        }
        let row: Option<(String, String)> = self
            .traversal
            .relations()
            .connection()
            .query_row(
                "SELECT path_rel, role FROM resource WHERE uid = ?1 AND state = 'ACTIVE'",
                params![resource.to_bytes().to_vec()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let owner = match row {
            Some((path_rel, role)) => {
                // The Resource model is the test authority; this only
                // reads what it already decided.
                let role = ResourceRole::parse_public(&role)?;
                if role == ResourceRole::Unknown {
                    coverage.unknown_role += 1;
                }
                Some(Owner {
                    id: resource,
                    path_rel,
                    role,
                })
            }
            None => {
                coverage.unresolved_owner += 1;
                None
            }
        };
        owners.insert(resource, owner.clone());
        Ok(owner)
    }
}

type OwnerCache = HashMap<ResourceId, Option<Owner>>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Owner {
    id: ResourceId,
    path_rel: String,
    role: ResourceRole,
}

struct PartialCandidate {
    resource: ResourceId,
    path_rel: String,
    endpoints: Vec<GraphEndpoint>,
    paths: Vec<TestPath>,
}

impl PartialCandidate {
    /// Order, bound, and summarise one Resource's paths.
    fn finish(mut self, coverage: &mut TestCoverage) -> RelatedTestCandidate {
        self.endpoints.sort_by_key(graph::endpoint_sort_key);
        self.endpoints.dedup();
        // Shortest first, then by identity, so the kept subset is the
        // same on every run and every machine.
        self.paths.sort_by(|left, right| {
            (left.length(), path_order(left)).cmp(&(right.length(), path_order(right)))
        });
        self.paths.dedup();

        let paths_truncated = self.paths.len() > MAX_SUPPORTING_PATHS;
        self.paths.truncate(MAX_SUPPORTING_PATHS);
        if paths_truncated {
            coverage.paths_truncated += 1;
        }

        let distance = self.paths.first().map_or(0, TestPath::length);
        let first_kind = self
            .paths
            .first()
            .and_then(|path| path.hops.first())
            .map(|hop| hop.kind);
        let basis = match (distance, first_kind) {
            (1, Some(kind)) => ProjectionBasis::DirectRelation(kind),
            (hops, Some(first)) => ProjectionBasis::RelationPath { hops, first },
            // Unreachable: a candidate exists only with a path.
            (hops, None) => ProjectionBasis::RelationPath {
                hops,
                first: RelationKind::References,
            },
        };

        let mut support = Support::Supported;
        let mut freshness = Freshness::Fresh;
        for hop in self.paths.iter().flat_map(|path| &path.hops) {
            support = weaker_support(support, hop.support);
            freshness = weaker_freshness(freshness, hop.freshness);
        }

        RelatedTestCandidate {
            resource: self.resource,
            path_rel: self.path_rel,
            endpoints: self.endpoints,
            distance,
            basis,
            paths: self.paths,
            paths_truncated,
            support,
            freshness,
        }
    }
}

/// The traversal's own facts, indexed for path reconstruction.
///
/// Built from the [`ImpactResult`] alone -- no query, no second walk.
struct PathIndex<'a> {
    /// Where each endpoint was first reached from, and along which kind.
    via: HashMap<&'a GraphEndpoint, (&'a GraphEndpoint, RelationKind)>,
    /// Canonical edges by identity.
    edges: HashMap<(RelationKind, &'a GraphEndpoint, &'a GraphEndpoint), &'a ImpactEdge>,
    /// Edges leaving each endpoint, in traversal order.
    out: HashMap<&'a GraphEndpoint, Vec<&'a ImpactEdge>>,
}

impl<'a> PathIndex<'a> {
    fn of(impact: &'a ImpactResult) -> Self {
        let mut via = HashMap::new();
        for node in &impact.nodes {
            if let Some((from, kind)) = &node.via {
                via.insert(&node.endpoint, (from, *kind));
            }
        }
        let mut edges = HashMap::new();
        let mut out: HashMap<&GraphEndpoint, Vec<&ImpactEdge>> = HashMap::new();
        for edge in &impact.edges {
            edges.insert(
                (
                    edge.relation.kind,
                    &edge.relation.source,
                    &edge.relation.target,
                ),
                edge,
            );
            out.entry(&edge.relation.source).or_default().push(edge);
        }
        Self { via, edges, out }
    }

    /// Every confirmed path from `endpoint` back to the root.
    ///
    /// One per edge leaving the endpoint that lands on something the
    /// traversal reached: the rest of the chain is the back-pointer
    /// path from there, which is why two paths to one test cost no
    /// second walk.
    fn paths_from(&self, endpoint: &GraphEndpoint, depth: usize) -> Vec<TestPath> {
        let mut found = Vec::new();
        for edge in self.out.get(endpoint).into_iter().flatten() {
            let mut hops = vec![edge.relation.clone()];
            if self.extend_to_root(&edge.relation.target, &mut hops, depth) {
                found.push(TestPath { hops });
            }
        }
        found
    }

    /// Walk the back-pointers from `from` to the root, appending each
    /// edge. `false` if the chain is not complete, in which case there
    /// is nothing confirmed to claim.
    fn extend_to_root(
        &self,
        from: &GraphEndpoint,
        hops: &mut Vec<RelationResult>,
        limit: usize,
    ) -> bool {
        let mut current = from;
        // The back-pointer chain is acyclic by construction; the limit
        // is belt and braces against a malformed result.
        for _ in 0..=limit {
            let Some((parent, kind)) = self.via.get(current) else {
                // No back-pointer: this is the root.
                return true;
            };
            let Some(edge) = self.edges.get(&(*kind, current, *parent)) else {
                return false;
            };
            hops.push(edge.relation.clone());
            current = parent;
        }
        false
    }
}

type PathOrder = Vec<(&'static str, (u8, Vec<u8>), (u8, Vec<u8>))>;

/// A stable order over a path, by canonical identity only.
fn path_order(path: &TestPath) -> PathOrder {
    path.hops
        .iter()
        .map(|hop| {
            (
                hop.kind.as_str(),
                graph::endpoint_sort_key(&hop.source),
                graph::endpoint_sort_key(&hop.target),
            )
        })
        .collect()
}

fn sixteen(raw: &[u8]) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    let take = raw.len().min(16);
    bytes[..take].copy_from_slice(&raw[..take]);
    bytes
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use brainprint_core::SymbolId;

    use super::*;
    use crate::{
        config::WorkspaceConfig,
        evidence::{OccurrenceRef, RelationEvidence, replace_resource_graph},
        gaps::{IntendedRelation, UnresolvedEvidence, UnresolvedReason},
        generation,
        graph::{GraphStore, Relation},
        resolution::{Dispatch, EvidenceBasis},
        resource::ResourceStore,
        scan::BaselineScan,
        symbol::{OccurrenceKind, SymbolStore},
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    const CALC_TS: &str = "\
export function calc(): number {
  return 1
}

export class Widget {}
";

    /// Not a test: the same relations from a SOURCE Resource.
    const CONSUMER_TS: &str = "\
import { calc } from '../src/calc'

export function consume(): number {
  return calc()
}
";

    /// A TEST Resource that calls and references the target.
    const CALC_TEST_TS: &str = "\
import { calc } from '../src/calc'

export function runs(): number {
  register(calc)
  return calc()
}
";

    /// A TEST Resource two confirmed hops away.
    const INDIRECT_TEST_TS: &str = "\
import { consume } from '../src/consumer'

export function indirect(): number {
  return consume()
}
";

    /// A TEST Resource whose name resembles the target's and which has
    /// no confirmed relation to it at all.
    const CALC_EXTRA_TEST_TS: &str = "\
export function extra(): number {
  return 2
}
";

    /// A TEST Resource whose only mentions are unresolved.
    const GAP_TEST_TS: &str = "\
import { thing } from './missing'

export function gapped(obj: Thing): number {
  obj.calc()
  return 1
}
";

    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-related-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            fs::create_dir_all(root.join("src")).expect("src");
            fs::create_dir_all(root.join("tests")).expect("tests");
            let fixture = Self { base, root };
            fixture.write("src/calc.ts", CALC_TS);
            fixture.write("src/consumer.ts", CONSUMER_TS);
            fixture.write("tests/calc.test.ts", CALC_TEST_TS);
            fixture.write("tests/indirect.test.ts", INDIRECT_TEST_TS);
            fixture.write("tests/calc_extra.test.ts", CALC_EXTRA_TEST_TS);
            fixture.write("tests/gap.test.ts", GAP_TEST_TS);
            BaselineScan::open(&fixture.db_path())
                .expect("index.db")
                .run_initial_scan(
                    &fixture.root,
                    &WorkspaceConfig::default(),
                    "workspace-rev-1",
                )
                .expect("baseline scan");
            fixture
        }

        fn db_path(&self) -> PathBuf {
            self.base.join("data").join("index.db")
        }

        fn write(&self, rel: &str, contents: &str) {
            fs::write(self.root.join(rel), contents).expect("fixture file");
        }

        fn resource(&self, rel: &str) -> crate::resource::Resource {
            ResourceStore::open(&self.db_path())
                .expect("index.db")
                .get_active_by_path_key(rel)
                .expect("lookup")
                .expect("the fixture file is a Resource")
        }

        fn file(&self, rel: &str) -> GraphEndpoint {
            GraphEndpoint::Resource(self.resource(rel).id)
        }

        fn symbol(&self, rel: &str, qualified_name: &str) -> SymbolId {
            SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_for_resource(self.resource(rel).id)
                .expect("symbols")
                .into_iter()
                .find(|symbol| symbol.qualified_name == qualified_name)
                .expect("the declaration is indexed")
                .id
        }

        fn declaration(&self, rel: &str, qualified_name: &str) -> GraphEndpoint {
            GraphEndpoint::Symbol(self.symbol(rel, qualified_name))
        }

        fn sites(&self, rel: &str, kind: OccurrenceKind) -> Vec<OccurrenceRef> {
            SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_occurrences_for_resource(self.resource(rel).id)
                .expect("occurrences")
                .into_iter()
                .filter(|occurrence| occurrence.kind == kind)
                .map(|occurrence| OccurrenceRef {
                    kind: occurrence.kind,
                    start_byte: occurrence.span.start_byte,
                    end_byte: occurrence.span.end_byte,
                })
                .collect()
        }

        fn specifiers(&self, rel: &str) -> Vec<OccurrenceRef> {
            let source = fs::read_to_string(self.root.join(rel)).expect("source");
            self.sites(rel, OccurrenceKind::ImportSite)
                .into_iter()
                .filter(|site| source[site.start_byte..site.end_byte].starts_with('\''))
                .collect()
        }

        fn import_names(&self, rel: &str) -> Vec<OccurrenceRef> {
            let source = fs::read_to_string(self.root.join(rel)).expect("source");
            self.sites(rel, OccurrenceKind::ImportSite)
                .into_iter()
                .filter(|site| !source[site.start_byte..site.end_byte].starts_with('\''))
                .collect()
        }

        fn projector(&self) -> RelatedTests {
            RelatedTests::open(&self.db_path()).expect("index.db")
        }

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

        /// Confirmed evidence for every file, and -- when asked -- the
        /// unresolved mentions that must never become test linkage.
        fn publish_baseline(&self, with_gaps: bool) {
            let calc_file = self.file("src/calc.ts");
            let consumer_file = self.file("src/consumer.ts");
            let calc_test_file = self.file("tests/calc.test.ts");
            let indirect_file = self.file("tests/indirect.test.ts");
            let calc = self.declaration("src/calc.ts", "calc");
            let widget = self.declaration("src/calc.ts", "Widget");
            let consume = self.declaration("src/consumer.ts", "consume");
            let runs = self.declaration("tests/calc.test.ts", "runs");
            let indirect = self.declaration("tests/indirect.test.ts", "indirect");

            let store = GraphStore::open(&self.db_path()).expect("index.db");
            let connection = store.connection();
            let revision = generation::current_workspace_revision(connection)
                .expect("clock")
                .expect("bootstrapped");
            let building = generation::begin_generation(connection, &revision).expect("begin");
            let transaction = connection.unchecked_transaction().expect("transaction");
            let (record, grant) =
                generation::grant_publication(&transaction, building.id).expect("grant");
            let published = building.id;

            let consumer_calls = self.sites("src/consumer.ts", OccurrenceKind::CallSite);
            let test_calls = self.sites("tests/calc.test.ts", OccurrenceKind::CallSite);
            let test_refs = self.sites("tests/calc.test.ts", OccurrenceKind::ReferenceSite);
            let indirect_calls = self.sites("tests/indirect.test.ts", OccurrenceKind::CallSite);

            let mut plan: Vec<(&str, Vec<RelationEvidence>, Vec<UnresolvedEvidence>)> = vec![
                (
                    "src/consumer.ts",
                    vec![
                        evidence(
                            consumer_calls[0],
                            edge(RelationKind::Calls, &consume, &calc, published),
                        ),
                        evidence(
                            self.specifiers("src/consumer.ts")[0],
                            edge(RelationKind::Imports, &consumer_file, &calc_file, published),
                        ),
                    ],
                    Vec::new(),
                ),
                (
                    "tests/calc.test.ts",
                    vec![
                        evidence(
                            test_calls[1],
                            edge(RelationKind::Calls, &runs, &calc, published),
                        ),
                        evidence(
                            test_refs[0],
                            edge(RelationKind::References, &runs, &calc, published),
                        ),
                        evidence(
                            self.specifiers("tests/calc.test.ts")[0],
                            edge(
                                RelationKind::Imports,
                                &calc_test_file,
                                &calc_file,
                                published,
                            ),
                        ),
                    ],
                    Vec::new(),
                ),
                (
                    "tests/indirect.test.ts",
                    vec![
                        evidence(
                            indirect_calls[0],
                            edge(RelationKind::Calls, &indirect, &consume, published),
                        ),
                        evidence(
                            self.specifiers("tests/indirect.test.ts")[0],
                            edge(
                                RelationKind::Imports,
                                &indirect_file,
                                &consumer_file,
                                published,
                            ),
                        ),
                    ],
                    Vec::new(),
                ),
            ];

            if with_gaps {
                plan.push((
                    "tests/gap.test.ts",
                    Vec::new(),
                    vec![
                        // Names `calc`, proves nothing about it.
                        UnresolvedEvidence {
                            occurrence: self.sites("tests/gap.test.ts", OccurrenceKind::CallSite)
                                [0],
                            intended: IntendedRelation::Known(RelationKind::Calls),
                            lookup_name: "calc".to_owned(),
                            module_hint: None,
                            reason: UnresolvedReason::ReceiverTypeRequired,
                            candidates: Vec::new(),
                        },
                        // Candidates in hand, and they stay candidates.
                        UnresolvedEvidence {
                            occurrence: self.sites("tests/gap.test.ts", OccurrenceKind::TypeSite)
                                [0],
                            intended: IntendedRelation::Known(RelationKind::UsesType),
                            lookup_name: "Thing".to_owned(),
                            module_hint: None,
                            reason: UnresolvedReason::AmbiguousCandidates,
                            candidates: vec![calc.clone()],
                        },
                        UnresolvedEvidence {
                            occurrence: self.import_names("tests/gap.test.ts")[0],
                            intended: IntendedRelation::Known(RelationKind::UsesType),
                            lookup_name: "Widget".to_owned(),
                            module_hint: None,
                            reason: UnresolvedReason::TypeSemanticsRequired,
                            candidates: vec![widget.clone()],
                        },
                    ],
                ));
            }

            for (rel, resolved, unresolved) in plan {
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
                    &self.basis(rel, published),
                    &resolved,
                    &unresolved,
                )
                .expect("replace");
            }
            generation::finish_publish_stable(&transaction, &record).expect("stable");
            transaction.commit().expect("commit");
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn edge(
        kind: RelationKind,
        source: &GraphEndpoint,
        target: &GraphEndpoint,
        generation: i64,
    ) -> Relation {
        Relation {
            kind,
            source: source.clone(),
            target: target.clone(),
            dispatch: Dispatch::Static,
            created_generation: generation,
        }
    }

    fn evidence(occurrence: OccurrenceRef, relation: Relation) -> RelationEvidence {
        RelationEvidence {
            occurrence,
            relation,
        }
    }

    fn generous() -> Budget {
        Budget {
            max_nodes: 100,
            max_edges: 100,
            max_depth: 10,
            time_limit: std::time::Duration::from_secs(30),
        }
    }

    fn paths_of(projection: &RelatedTestProjection, rel: &str) -> Vec<String> {
        projection
            .candidates
            .iter()
            .filter(|candidate| candidate.path_rel == rel)
            .flat_map(|candidate| &candidate.paths)
            .map(|path| {
                path.hops
                    .iter()
                    .map(|hop| hop.kind.as_str())
                    .collect::<Vec<_>>()
                    .join(">")
            })
            .collect()
    }

    fn files_of(projection: &RelatedTestProjection) -> Vec<String> {
        projection
            .candidates
            .iter()
            .map(|candidate| candidate.path_rel.clone())
            .collect()
    }

    #[test]
    fn a_test_that_calls_and_references_the_target_is_projected_once() {
        let fixture = Fixture::create("direct");
        fixture.publish_baseline(false);
        let calc = fixture.declaration("src/calc.ts", "calc");

        let projection = fixture
            .projector()
            .for_target(&calc, ImpactIntent::Rename, &generous())
            .expect("project");

        assert_eq!(projection.outcome(), ProjectionOutcome::Candidates);
        let direct = projection
            .candidates
            .iter()
            .find(|candidate| candidate.path_rel == "tests/calc.test.ts")
            .expect("the test that calls calc");
        assert_eq!(direct.resource, fixture.resource("tests/calc.test.ts").id);
        assert_eq!(
            direct.endpoints,
            vec![fixture.declaration("tests/calc.test.ts", "runs")],
            "the test Symbol, not just its file"
        );
        assert_eq!(direct.distance, 1);
        assert!(direct.basis.is_direct());
        assert!(!direct.paths_truncated);

        // Two confirmed relations, one candidate -- and both kept as
        // the explanation.
        assert_eq!(
            files_of(&projection)
                .iter()
                .filter(|path| *path == "tests/calc.test.ts")
                .count(),
            1,
            "deduped to one candidate"
        );
        let mut kinds = paths_of(&projection, "tests/calc.test.ts");
        kinds.sort();
        assert_eq!(kinds, vec!["CALLS", "REFERENCES"]);

        // And the evidence behind them survived the projection.
        for path in &direct.paths {
            assert!(
                path.hops
                    .iter()
                    .all(|hop| !hop.evidence.is_empty() && hop.target == calc)
            );
        }
        assert_eq!(direct.support, Support::Supported);
        assert_eq!(direct.freshness, Freshness::Fresh);
    }

    #[test]
    fn a_non_test_resource_with_the_same_relations_is_not_a_candidate() {
        let fixture = Fixture::create("role");
        fixture.publish_baseline(false);
        let calc = fixture.declaration("src/calc.ts", "calc");

        let projection = fixture
            .projector()
            .for_target(&calc, ImpactIntent::Rename, &generous())
            .expect("project");

        assert!(
            !files_of(&projection).contains(&"src/consumer.ts".to_owned()),
            "consumer.ts calls calc too, and is not a test"
        );
        assert_eq!(
            fixture.resource("src/consumer.ts").role,
            ResourceRole::Source,
            "the Resource model says so, and this module does not argue"
        );
    }

    #[test]
    fn a_similarly_named_test_without_a_confirmed_relation_is_not_a_candidate() {
        let fixture = Fixture::create("no-name-match");
        fixture.publish_baseline(false);
        let calc = fixture.declaration("src/calc.ts", "calc");

        let projection = fixture
            .projector()
            .for_target(&calc, ImpactIntent::Rename, &generous())
            .expect("project");

        assert_eq!(
            fixture.resource("tests/calc_extra.test.ts").role,
            ResourceRole::Test,
            "it is a test, and its name resembles the target's"
        );
        assert!(
            !files_of(&projection).contains(&"tests/calc_extra.test.ts".to_owned()),
            "a name is not evidence"
        );
        // Meanwhile a test whose name resembles nothing *is* returned,
        // because the graph says so.
        assert!(files_of(&projection).contains(&"tests/indirect.test.ts".to_owned()));
    }

    #[test]
    fn a_direct_candidate_ranks_before_a_longer_confirmed_path() {
        let fixture = Fixture::create("ranking");
        fixture.publish_baseline(false);
        let calc = fixture.declaration("src/calc.ts", "calc");

        let projection = fixture
            .projector()
            .for_target(&calc, ImpactIntent::Rename, &generous())
            .expect("project");

        assert_eq!(
            files_of(&projection),
            vec!["tests/calc.test.ts", "tests/indirect.test.ts"]
        );
        let indirect = &projection.candidates[1];
        assert_eq!(indirect.distance, 2);
        assert!(!indirect.basis.is_direct());
        assert_eq!(
            indirect.basis,
            ProjectionBasis::RelationPath {
                hops: 2,
                first: RelationKind::Calls
            }
        );
        // The chain is kept whole: test -> consumer -> calc.
        let hops = &indirect.paths[0].hops;
        assert_eq!(hops.len(), 2);
        assert_eq!(
            hops[0].source,
            fixture.declaration("tests/indirect.test.ts", "indirect")
        );
        assert_eq!(hops[1].target, calc);
    }

    #[test]
    fn confirmed_imports_project_a_test_for_a_module_change() {
        let fixture = Fixture::create("imports");
        fixture.publish_baseline(false);
        let calc_file = fixture.file("src/calc.ts");

        let projection = fixture
            .projector()
            .for_target(&calc_file, ImpactIntent::ModuleMove, &generous())
            .expect("project");

        assert_eq!(
            files_of(&projection),
            vec!["tests/calc.test.ts", "tests/indirect.test.ts"]
        );
        let direct = &projection.candidates[0];
        assert_eq!(
            direct.basis,
            ProjectionBasis::DirectRelation(RelationKind::Imports)
        );
        assert_eq!(
            direct.endpoints,
            vec![fixture.file("tests/calc.test.ts")],
            "a file-level import names the Resource, and no Symbol is invented"
        );
        assert_eq!(
            direct.paths[0].hops[0].evidence[0].occurrence_kind,
            OccurrenceKind::ImportSite
        );
    }

    #[test]
    fn unresolved_and_candidate_mentions_never_become_test_linkage() {
        let fixture = Fixture::create("gaps");
        fixture.publish_baseline(true);
        let calc = fixture.declaration("src/calc.ts", "calc");

        let projection = fixture
            .projector()
            .for_target(&calc, ImpactIntent::Rename, &generous())
            .expect("project");

        assert!(
            !files_of(&projection).contains(&"tests/gap.test.ts".to_owned()),
            "an unresolved mention of `calc` is not a confirmed test relation"
        );
        // The gap is still reported -- as a gap.
        assert!(projection.coverage.impact.gaps > 0 || projection.coverage.impact.unattributed > 0);
        assert!(
            projection.coverage.impact.ambiguous > 0,
            "a candidate stayed a candidate"
        );
        assert!(!projection.coverage.is_complete());
        assert_eq!(
            projection.outcome(),
            ProjectionOutcome::Candidates,
            "the confirmed ones are still returned"
        );
    }

    #[test]
    fn an_empty_result_says_whether_the_coverage_behind_it_was_complete() {
        // Complete: nothing unresolved anywhere, and nothing points at
        // Widget.
        let clean = Fixture::create("complete-zero");
        clean.publish_baseline(false);
        let widget = clean.declaration("src/calc.ts", "Widget");
        let confirmed_zero = clean
            .projector()
            .for_target(&widget, ImpactIntent::Rename, &generous())
            .expect("project");
        assert!(confirmed_zero.candidates.is_empty());
        assert!(confirmed_zero.coverage.is_complete());
        assert_eq!(
            confirmed_zero.outcome(),
            ProjectionOutcome::NoneUnderCompleteCoverage
        );

        // Incomplete: an unresolved use names Widget as a candidate.
        let gapped = Fixture::create("incomplete-zero");
        gapped.publish_baseline(true);
        let widget = gapped.declaration("src/calc.ts", "Widget");
        let unknown_zero = gapped
            .projector()
            .for_target(&widget, ImpactIntent::Rename, &generous())
            .expect("project");
        assert!(unknown_zero.candidates.is_empty());
        assert!(!unknown_zero.coverage.is_complete());
        assert_eq!(
            unknown_zero.outcome(),
            ProjectionOutcome::NoneWithIncompleteCoverage,
            "zero found is not zero existing"
        );
        assert!(
            unknown_zero.coverage.impact.requires_semantics > 0
                || unknown_zero.coverage.impact.gaps > 0
        );
    }

    #[test]
    fn traversal_truncation_is_carried_into_test_coverage() {
        let fixture = Fixture::create("truncated");
        fixture.publish_baseline(false);
        let calc = fixture.declaration("src/calc.ts", "calc");

        let projection = fixture
            .projector()
            .for_target(
                &calc,
                ImpactIntent::Rename,
                &Budget {
                    max_nodes: 2,
                    ..generous()
                },
            )
            .expect("project");

        assert_eq!(
            projection.coverage.traversal_truncation,
            Some(Truncation::NodeBudget)
        );
        assert!(projection.coverage.impact.truncated);
        assert!(!projection.coverage.is_complete());
        assert!(
            !files_of(&projection).contains(&"tests/indirect.test.ts".to_owned()),
            "the budget stopped before it"
        );
    }

    #[test]
    fn degraded_evidence_prevents_a_complete_claim() {
        let fixture = Fixture::create("degraded");
        fixture.publish_baseline(false);
        let calc = fixture.declaration("src/calc.ts", "calc");

        // The test file moves on: its evidence is the last valid one.
        let store = GraphStore::open(&fixture.db_path()).expect("index.db");
        store
            .connection()
            .execute(
                "UPDATE resource SET resource_revision = 'moved' WHERE uid = ?1",
                params![
                    fixture
                        .resource("tests/calc.test.ts")
                        .id
                        .to_bytes()
                        .to_vec()
                ],
            )
            .expect("bump revision");
        drop(store);

        let projection = fixture
            .projector()
            .for_target(&calc, ImpactIntent::Rename, &generous())
            .expect("project");

        let direct = projection
            .candidates
            .iter()
            .find(|candidate| candidate.path_rel == "tests/calc.test.ts")
            .expect("still a candidate");
        assert_eq!(direct.freshness, Freshness::Stale);
        assert!(projection.coverage.impact.stale_evidence > 0);
        assert!(
            !projection.coverage.is_complete(),
            "stale evidence is not current coverage"
        );
    }

    #[test]
    fn an_impact_result_projects_without_walking_the_graph_again() {
        let fixture = Fixture::create("reuse");
        fixture.publish_baseline(false);
        let calc = fixture.declaration("src/calc.ts", "calc");
        let projector = fixture.projector();

        let impact = projector
            .traversal()
            .run(ImpactIntent::Rename, &calc, &generous())
            .expect("traverse");
        let from_traversal = projector.from_impact(&impact).expect("project");

        // Every relation row goes away. A projection that re-walked the
        // graph would now find nothing; this one still has the paths
        // task 10 already confirmed.
        let store = GraphStore::open(&fixture.db_path()).expect("index.db");
        store
            .connection()
            .execute("UPDATE occurrence SET relation_id = NULL", [])
            .expect("unbind");
        store
            .connection()
            .execute("DELETE FROM relation", [])
            .expect("clear");
        drop(store);

        let reprojected = projector.from_impact(&impact).expect("project");
        assert_eq!(reprojected, from_traversal);
        assert_eq!(
            files_of(&reprojected),
            vec!["tests/calc.test.ts", "tests/indirect.test.ts"]
        );

        // Whereas a fresh traversal now genuinely has nothing.
        let empty = projector
            .for_target(&calc, ImpactIntent::Rename, &generous())
            .expect("project");
        assert!(empty.candidates.is_empty());
    }

    #[test]
    fn projection_is_deterministic_and_writes_no_relation() {
        let fixture = Fixture::create("deterministic");
        fixture.publish_baseline(true);
        let projector = fixture.projector();
        let calc = fixture.declaration("src/calc.ts", "calc");

        let before = stored_kinds(&fixture);
        let first = projector
            .for_target(&calc, ImpactIntent::Rename, &generous())
            .expect("project");
        let second = projector
            .for_target(&calc, ImpactIntent::Rename, &generous())
            .expect("project");
        let after = stored_kinds(&fixture);

        assert_eq!(first, second, "same state, same projection");
        assert_eq!(before, after, "a projection is not a write");
        for forbidden in ["RELATED_TEST", "TESTED_BY", "TESTS"] {
            assert!(
                !after.iter().any(|kind| kind == forbidden),
                "{forbidden} row exists"
            );
            assert!(
                RelationKind::parse(forbidden).is_err(),
                "{forbidden} is not a relation kind"
            );
        }
        assert!(
            after.iter().all(|kind| RelationKind::parse(kind).is_ok()),
            "only P0 canonical kinds are stored: {after:?}"
        );
    }

    #[test]
    fn a_projection_carries_locators_and_no_source_body() {
        let fixture = Fixture::create("no-source");
        fixture.publish_baseline(false);
        let calc = fixture.declaration("src/calc.ts", "calc");

        let projection = fixture
            .projector()
            .for_target(&calc, ImpactIntent::Rename, &generous())
            .expect("project");
        let rendered = format!("{projection:?}");

        for body in ["return calc()", "export function runs"] {
            assert!(!rendered.contains(body), "{body:?} leaked into the result");
        }
        let location = &projection.candidates[0].paths[0].hops[0].evidence[0];
        assert!(location.span.end_byte > location.span.start_byte);

        let stored = fs::read(fixture.db_path()).expect("index.db");
        for body in ["return calc()", "export function runs"] {
            assert!(
                !stored
                    .windows(body.len())
                    .any(|window| window == body.as_bytes()),
                "index.db mirrors {body:?}"
            );
        }
    }

    fn stored_kinds(fixture: &Fixture) -> Vec<String> {
        let store = GraphStore::open(&fixture.db_path()).expect("index.db");
        let mut statement = store
            .connection()
            .prepare("SELECT kind FROM relation ORDER BY kind")
            .expect("prepare");
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query");
        rows.map(|row| row.expect("kind")).collect()
    }
}
