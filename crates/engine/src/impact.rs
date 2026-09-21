//! Typed, bounded impact traversal (#17 task 10).
//!
//! Task 8 answers one hop. A change is not one hop: renaming a function
//! reaches its callers, and their callers. This walks that out -- but
//! only along the edges the *kind of change* justifies, and only inside
//! budgets that are reported rather than silently stretched.
//!
//! ## Typed intents, not `depth=N`
//!
//! The public API takes an [`ImpactIntent`], never a graph pattern. Each
//! intent is a fixed plan of `(RelationKind, direction)` steps
//! ([`ImpactIntent::plan`]): a signature change follows CALLS,
//! OVERRIDES, IMPLEMENTS and USES_TYPE; a rename follows REFERENCES,
//! CALLS, IMPORTS and USES_TYPE; a module move follows IMPORTS; a
//! base/interface change follows EXTENDS, IMPLEMENTS, OVERRIDES and
//! USES_TYPE. Nothing else is walked, in either direction.
//!
//! Direction is part of the plan and is always *incoming*: impact flows
//! from the thing that changed to the things that depend on it. A
//! traversal never walks outgoing edges "as well" -- what a caller calls
//! is not affected by that caller changing.
//!
//! ## Safety, and what a budget means
//!
//! Every endpoint is expanded at most once, so a cycle terminates; a
//! canonical edge is emitted at most once, however many paths reach it.
//! Node, edge, depth and elapsed-time budgets are all enforced, and
//! hitting one is [`Truncation`] -- **not** completion, and not the same
//! thing as task 7's candidate truncation or task 8's unresolved
//! coverage. All of those stay separately visible.
//!
//! A truncated traversal hands back a [`Continuation`]: the frontier it
//! had left, plus what it has already emitted, so resuming continues
//! exactly where it stopped without re-emitting a node or an edge and
//! without losing cycle correctness. A continuation is bound to the root
//! and intent that produced it and is refused anywhere else. It carries
//! stable identities only -- no row ids, no source.
//!
//! ## What this is not
//!
//! Related tests are task 11, env/config relations task 12, lifecycle
//! recovery task 13, and Context Projection/MCP I5. There is no generic
//! graph query language here and no unrestricted dump: without an
//! intent there is no traversal.

use std::{
    collections::HashSet,
    error::Error,
    fmt,
    path::Path,
    time::{Duration, Instant},
};

use brainprint_core::ResourceId;

use crate::{
    graph::{self, GraphEndpoint, RelationKind},
    relations::{Direction, RelationError, RelationGap, RelationIndex, RelationResult},
    resolution::{Freshness, Support},
};

/// The kind of change whose impact is being traced.
///
/// A closed vocabulary: the plans are the point. An arbitrary set of
/// kinds is not accepted, because "which edges matter" is a property of
/// the change, not a knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImpactIntent {
    /// A public signature changed: everything that calls it, overrides
    /// it, implements it, or names it as a type.
    PublicSignatureChange,
    /// A Symbol is being renamed: every written mention of the name.
    Rename,
    /// A module moved or was renamed: everything that imports it, plus
    /// the module gaps that could be attributed.
    ModuleMove,
    /// A base class or interface changed: the hierarchy under it, and
    /// what names it as a type.
    BaseInterfaceChange,
}

/// One step of an intent's plan: a relation kind, and the direction
/// impact flows along it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanStep {
    pub kind: RelationKind,
    pub direction: Direction,
}

impl ImpactIntent {
    /// The fixed plan this intent traverses.
    #[must_use]
    pub fn plan(self) -> Vec<PlanStep> {
        let kinds: &[RelationKind] = match self {
            Self::PublicSignatureChange => &[
                RelationKind::Calls,
                RelationKind::Overrides,
                RelationKind::Implements,
                RelationKind::UsesType,
            ],
            Self::Rename => &[
                RelationKind::References,
                RelationKind::Calls,
                RelationKind::Imports,
                RelationKind::UsesType,
            ],
            Self::ModuleMove => &[RelationKind::Imports],
            Self::BaseInterfaceChange => &[
                RelationKind::Extends,
                RelationKind::Implements,
                RelationKind::Overrides,
                RelationKind::UsesType,
            ],
        };
        kinds
            .iter()
            .map(|kind| PlanStep {
                kind: *kind,
                // Impact flows from what changed to what depends on it.
                // The other direction is a different question.
                direction: Direction::Incoming,
            })
            .collect()
    }

    /// The kinds this intent follows.
    #[must_use]
    pub fn kinds(self) -> Vec<RelationKind> {
        self.plan().into_iter().map(|step| step.kind).collect()
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PublicSignatureChange => "PUBLIC_SIGNATURE_CHANGE",
            Self::Rename => "RENAME",
            Self::ModuleMove => "MODULE_MOVE",
            Self::BaseInterfaceChange => "BASE_INTERFACE_CHANGE",
        }
    }

    /// What a continuation is bound to, beyond root and intent: if a
    /// plan ever changes, an old continuation stops being resumable
    /// rather than resuming into different semantics.
    fn plan_fingerprint(self) -> String {
        let steps: Vec<String> = self
            .plan()
            .into_iter()
            .map(|step| {
                format!(
                    "{}:{}",
                    step.kind.as_str(),
                    match step.direction {
                        Direction::Incoming => "IN",
                        Direction::Outgoing => "OUT",
                    }
                )
            })
            .collect();
        format!("{}|{}", self.as_str(), steps.join(","))
    }
}

impl fmt::Display for ImpactIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What a traversal may spend before it must stop and say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub max_nodes: usize,
    pub max_edges: usize,
    /// The greatest depth a node may be reached at, the root being 0.
    /// An endpoint at exactly this depth is returned but not expanded,
    /// and that is a [`Truncation::DepthBudget`], not a finished walk.
    pub max_depth: usize,
    pub time_limit: Duration,
}

impl Default for Budget {
    /// Enough for a real change review, small enough that a pathological
    /// graph cannot run away. Every value is the caller's to raise --
    /// explicitly, which is the point.
    fn default() -> Self {
        Self {
            max_nodes: 200,
            max_edges: 500,
            max_depth: 3,
            time_limit: Duration::from_secs(2),
        }
    }
}

/// Which budget stopped the walk.
///
/// Deliberately not mixed with coverage: this says the traversal was
/// cut short, never that the graph has nothing more in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Truncation {
    NodeBudget,
    EdgeBudget,
    DepthBudget,
    TimeBudget,
}

/// What the walk actually spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BudgetUsage {
    pub nodes: usize,
    pub edges: usize,
    /// The deepest node reached, root being 0.
    pub depth: usize,
    /// Endpoints whose relations were queried.
    pub expanded: usize,
}

/// One endpoint the traversal reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactNode {
    pub endpoint: GraphEndpoint,
    /// Hops from the root.
    pub depth: usize,
    /// The endpoint it was first reached from, and along which kind.
    /// `None` for the root. Following these back gives the path, without
    /// storing one path per node.
    pub via: Option<(GraphEndpoint, RelationKind)>,
    /// Whether this endpoint's own relations were queried. False for a
    /// node the budget stopped short of.
    pub expanded: bool,
}

/// One canonical edge the traversal followed, with task 8's contract
/// intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactEdge {
    /// Task 8's result: endpoints, kind, direction, dispatch, scope,
    /// resolution, support, freshness, and every exact evidence
    /// location.
    pub relation: RelationResult,
    /// The depth of the endpoint this edge was found at.
    pub depth: usize,
}

/// A gap, and the traversed endpoint it was attributed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributedGap {
    /// The endpoint whose query surfaced it. Attribution is task 8's --
    /// a candidate identity, never a name match.
    pub at: GraphEndpoint,
    pub depth: usize,
    pub gap: RelationGap,
}

/// What the traversal could not confirm, alongside what it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImpactCoverage {
    /// Gaps attributed to a traversed endpoint.
    pub gaps: usize,
    pub ambiguous: usize,
    pub requires_semantics: usize,
    pub unsupported_construct: usize,
    /// Gaps whose *candidate list* was cut (#17 task 7). Nothing to do
    /// with [`Truncation`].
    pub candidate_truncated: usize,
    /// Unresolved sites that could not be attributed to any traversed
    /// endpoint. Counted, never attached.
    pub unattributed: usize,
    /// Emitted edges whose evidence is only partially covered.
    pub partial_support: usize,
    /// Emitted edges whose evidence is stale.
    pub stale_evidence: usize,
    /// Emitted edges whose evidence is dirty.
    pub dirty_evidence: usize,
    /// Whether the walk ran out of budget.
    pub truncated: bool,
}

impl ImpactCoverage {
    /// Whether this traversal may be read as the whole impact.
    ///
    /// False for a budget truncation, for any gap, and for degraded
    /// evidence -- three different reasons, none of which is allowed to
    /// hide behind the others.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        !self.truncated
            && self.gaps == 0
            && self.unattributed == 0
            && self.partial_support == 0
            && self.stale_evidence == 0
            && self.dirty_evidence == 0
    }
}

/// Enough state to resume a truncated traversal exactly where it
/// stopped.
///
/// Stable identities only: endpoints and relation kinds. No row ids, no
/// source, and never the whole graph -- what it holds is bounded by the
/// budget that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Continuation {
    root: GraphEndpoint,
    intent: ImpactIntent,
    plan_fingerprint: String,
    /// Still to expand, in deterministic order, with their depths.
    frontier: Vec<(GraphEndpoint, usize)>,
    /// Already returned, so a resume does not return them again.
    emitted_nodes: Vec<GraphEndpoint>,
    emitted_edges: Vec<(RelationKind, GraphEndpoint, GraphEndpoint)>,
    /// Fully expanded already, so a cycle stays guarded across resumes.
    expanded: Vec<GraphEndpoint>,
    /// Gap anchors already reported, so a resume does not repeat one.
    emitted_gaps: Vec<GapAnchor>,
}

impl Continuation {
    #[must_use]
    pub const fn root(&self) -> &GraphEndpoint {
        &self.root
    }

    #[must_use]
    pub const fn intent(&self) -> ImpactIntent {
        self.intent
    }

    /// How many endpoints are still waiting to be expanded.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.frontier.len()
    }
}

/// One bounded impact traversal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactResult {
    pub intent: ImpactIntent,
    pub root: GraphEndpoint,
    /// The plan actually walked.
    pub plan: Vec<PlanStep>,
    /// Endpoints discovered by *this* run, in traversal order. A resume
    /// never repeats one.
    pub nodes: Vec<ImpactNode>,
    /// Canonical edges emitted by this run, in traversal order.
    pub edges: Vec<ImpactEdge>,
    pub gaps: Vec<AttributedGap>,
    pub coverage: ImpactCoverage,
    pub budget: BudgetUsage,
    /// Which budget stopped the walk, if one did.
    pub truncation: Option<Truncation>,
    /// Present exactly when [`Self::truncation`] is.
    pub continuation: Option<Continuation>,
}

impl ImpactResult {
    /// Whether the walk finished within budget.
    #[must_use]
    pub const fn is_complete_walk(&self) -> bool {
        self.truncation.is_none()
    }
}

/// Failure running or resuming a traversal.
#[derive(Debug)]
pub enum ImpactError {
    Relation(RelationError),
    /// A continuation from another root, intent, or plan. Resuming it
    /// would mix two traversals' semantics.
    ContinuationMismatch {
        field: &'static str,
        expected: String,
        found: String,
    },
}

impl fmt::Display for ImpactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Relation(error) => write!(formatter, "relation query: {error}"),
            Self::ContinuationMismatch {
                field,
                expected,
                found,
            } => write!(
                formatter,
                "continuation {field} is {found}, not the requested {expected}"
            ),
        }
    }
}

impl Error for ImpactError {}

impl From<RelationError> for ImpactError {
    fn from(error: RelationError) -> Self {
        Self::Relation(error)
    }
}

/// Bounded typed traversal over the relation graph.
pub struct ImpactTraversal {
    relations: RelationIndex,
}

impl ImpactTraversal {
    /// Open the `index.db` at `path`.
    pub fn open(path: &Path) -> Result<Self, ImpactError> {
        Ok(Self::new(RelationIndex::open(path)?))
    }

    /// Traverse over an already-opened relation query surface.
    #[must_use]
    pub const fn new(relations: RelationIndex) -> Self {
        Self { relations }
    }

    /// The direct query surface this walk is built from.
    #[must_use]
    pub const fn relations(&self) -> &RelationIndex {
        &self.relations
    }

    /// Trace one change's impact from `root`, within `budget`.
    pub fn run(
        &self,
        intent: ImpactIntent,
        root: &GraphEndpoint,
        budget: &Budget,
    ) -> Result<ImpactResult, ImpactError> {
        self.walk(intent, root, budget, None)
    }

    /// Continue a truncated traversal.
    ///
    /// The intent and root are restated so that a continuation cannot be
    /// fed to a different question: a mismatch is refused, not merged.
    /// The budget is the caller's again -- nothing is silently raised.
    pub fn resume(
        &self,
        intent: ImpactIntent,
        root: &GraphEndpoint,
        continuation: &Continuation,
        budget: &Budget,
    ) -> Result<ImpactResult, ImpactError> {
        if continuation.intent != intent {
            return Err(ImpactError::ContinuationMismatch {
                field: "intent",
                expected: intent.to_string(),
                found: continuation.intent.to_string(),
            });
        }
        if &continuation.root != root {
            return Err(ImpactError::ContinuationMismatch {
                field: "root",
                expected: format!("{root:?}"),
                found: format!("{:?}", continuation.root),
            });
        }
        if continuation.plan_fingerprint != intent.plan_fingerprint() {
            return Err(ImpactError::ContinuationMismatch {
                field: "plan",
                expected: intent.plan_fingerprint(),
                found: continuation.plan_fingerprint.clone(),
            });
        }
        self.walk(intent, root, budget, Some(continuation))
    }

    fn walk(
        &self,
        intent: ImpactIntent,
        root: &GraphEndpoint,
        budget: &Budget,
        resume: Option<&Continuation>,
    ) -> Result<ImpactResult, ImpactError> {
        let started = Instant::now();
        let kinds = intent.kinds();
        let mut state = WalkState::start(root, resume);

        let mut nodes: Vec<ImpactNode> = Vec::new();
        let mut edges: Vec<ImpactEdge> = Vec::new();
        let mut gaps: Vec<AttributedGap> = Vec::new();
        let mut coverage = ImpactCoverage::default();
        let mut deepest = 0;

        // A fresh walk emits its root; a resume already did.
        if state.emit_node(root.clone()) {
            nodes.push(ImpactNode {
                endpoint: root.clone(),
                depth: 0,
                via: None,
                expanded: false,
            });
        }

        let mut truncation = None;
        let mut depth_limited = false;
        while let Some((endpoint, depth)) = state.frontier.get(state.cursor).cloned() {
            if depth >= budget.max_depth {
                // Breadth first, so everything left is at least this
                // deep. The rest stays in the frontier for a resume
                // with more depth to spend.
                depth_limited = true;
                break;
            }
            if started.elapsed() > budget.time_limit {
                truncation = Some(Truncation::TimeBudget);
                break;
            }
            deepest = deepest.max(depth);

            let answer = self.relations.incoming(&endpoint, &kinds)?;
            let mut node_gaps = Vec::new();
            let mut stopped = None;
            for relation in &answer.confirmed {
                let key = (
                    relation.kind,
                    relation.source.clone(),
                    relation.target.clone(),
                );
                // One canonical edge, however many paths reach it. An
                // edge already emitted is still walked through: the
                // endpoint behind it may not have been reached yet.
                if state.emitted_edges.insert(key) {
                    if state.edge_count >= budget.max_edges {
                        stopped = Some(Truncation::EdgeBudget);
                        break;
                    }
                    state.edge_count += 1;
                    edges.push(ImpactEdge {
                        relation: relation.clone(),
                        depth,
                    });
                    count_evidence(&mut coverage, relation);
                }

                // Impact flows in from the source end of an incoming
                // edge; that is the next thing affected.
                let next = relation.source.clone();
                if state.emitted_nodes.contains(&next) {
                    continue;
                }
                if state.node_count >= budget.max_nodes {
                    stopped = Some(Truncation::NodeBudget);
                    break;
                }
                state.emit_node(next.clone());
                nodes.push(ImpactNode {
                    endpoint: next.clone(),
                    depth: depth + 1,
                    via: Some((endpoint.clone(), relation.kind)),
                    expanded: false,
                });
                deepest = deepest.max(depth + 1);
                state.frontier.push((next, depth + 1));
            }

            if let Some(reason) = stopped {
                // Stopped part-way through this endpoint. It stays
                // unexpanded, and a resume re-reads it -- already
                // emitted edges are skipped, so nothing repeats.
                truncation = Some(reason);
                break;
            }

            for gap in &answer.gaps {
                // One unresolved site is one gap, even when two
                // traversed endpoints both appear in its candidate
                // list. The first attribution wins, which is
                // deterministic in traversal order.
                if !state.emitted_gaps.insert(anchor_of(gap)) {
                    continue;
                }
                node_gaps.push(AttributedGap {
                    at: endpoint.clone(),
                    depth,
                    gap: gap.clone(),
                });
            }
            coverage.unattributed += answer.coverage.unattributed;
            count_gaps(&mut coverage, &node_gaps);
            gaps.extend(node_gaps);

            state.expanded.insert(endpoint.clone());
            mark_expanded(&mut nodes, &endpoint);
            state.cursor += 1;
        }

        // Nodes the depth budget stopped short of are the only thing
        // left: the walk is truncated, not finished.
        if truncation.is_none() && depth_limited {
            truncation = Some(Truncation::DepthBudget);
        }
        coverage.truncated = truncation.is_some();

        let continuation = truncation.map(|_| state.continuation(intent, root));
        Ok(ImpactResult {
            intent,
            root: root.clone(),
            plan: intent.plan(),
            budget: BudgetUsage {
                nodes: state.node_count,
                edges: state.edge_count,
                depth: deepest,
                expanded: state.expanded.len(),
            },
            nodes,
            edges,
            gaps,
            coverage,
            truncation,
            continuation,
        })
    }
}

/// The walk's dedupe and frontier state, seeded from a continuation
/// when there is one.
struct WalkState {
    frontier: Vec<(GraphEndpoint, usize)>,
    cursor: usize,
    emitted_nodes: HashSet<GraphEndpoint>,
    emitted_edges: HashSet<(RelationKind, GraphEndpoint, GraphEndpoint)>,
    emitted_gaps: HashSet<GapAnchor>,
    expanded: HashSet<GraphEndpoint>,
    node_count: usize,
    edge_count: usize,
}

impl WalkState {
    fn start(root: &GraphEndpoint, resume: Option<&Continuation>) -> Self {
        match resume {
            None => Self {
                frontier: vec![(root.clone(), 0)],
                cursor: 0,
                emitted_nodes: HashSet::new(),
                emitted_edges: HashSet::new(),
                emitted_gaps: HashSet::new(),
                expanded: HashSet::new(),
                node_count: 0,
                edge_count: 0,
            },
            Some(continuation) => Self {
                frontier: continuation.frontier.clone(),
                cursor: 0,
                emitted_nodes: continuation.emitted_nodes.iter().cloned().collect(),
                emitted_edges: continuation.emitted_edges.iter().cloned().collect(),
                emitted_gaps: continuation.emitted_gaps.iter().copied().collect(),
                expanded: continuation.expanded.iter().cloned().collect(),
                // Budgets are per run: what the previous run spent is
                // already returned, and the caller set this run's.
                node_count: 0,
                edge_count: 0,
            },
        }
    }

    /// Record an endpoint as returned. `false` if it already was.
    fn emit_node(&mut self, endpoint: GraphEndpoint) -> bool {
        if !self.emitted_nodes.insert(endpoint) {
            return false;
        }
        self.node_count += 1;
        true
    }

    fn continuation(&self, intent: ImpactIntent, root: &GraphEndpoint) -> Continuation {
        let mut emitted_nodes: Vec<GraphEndpoint> = self.emitted_nodes.iter().cloned().collect();
        emitted_nodes.sort_by_key(graph::endpoint_sort_key);
        let mut expanded: Vec<GraphEndpoint> = self.expanded.iter().cloned().collect();
        expanded.sort_by_key(graph::endpoint_sort_key);
        let mut emitted_edges: Vec<(RelationKind, GraphEndpoint, GraphEndpoint)> =
            self.emitted_edges.iter().cloned().collect();
        emitted_edges.sort_by(|left, right| edge_order(left).cmp(&edge_order(right)));
        let mut emitted_gaps: Vec<GapAnchor> = self.emitted_gaps.iter().copied().collect();
        emitted_gaps.sort_unstable();
        Continuation {
            root: root.clone(),
            intent,
            plan_fingerprint: intent.plan_fingerprint(),
            emitted_gaps,
            // What is left, in the order it would have been walked.
            frontier: self.frontier[self.cursor.min(self.frontier.len())..].to_vec(),
            emitted_nodes,
            emitted_edges,
            expanded,
        }
    }
}

/// One unresolved site, by the Occurrence it is anchored to. Stable
/// identity and byte range -- never a row id.
type GapAnchor = (ResourceId, usize, usize);

fn anchor_of(gap: &RelationGap) -> GapAnchor {
    (
        gap.location.resource,
        gap.location.span.start_byte,
        gap.location.span.end_byte,
    )
}

type EdgeOrder = (&'static str, (u8, Vec<u8>), (u8, Vec<u8>));

/// A stable order for a continuation's edge set, over canonical
/// identity only.
fn edge_order(edge: &(RelationKind, GraphEndpoint, GraphEndpoint)) -> EdgeOrder {
    (
        edge.0.as_str(),
        graph::endpoint_sort_key(&edge.1),
        graph::endpoint_sort_key(&edge.2),
    )
}

fn mark_expanded(nodes: &mut [ImpactNode], endpoint: &GraphEndpoint) {
    if let Some(node) = nodes.iter_mut().find(|node| &node.endpoint == endpoint) {
        node.expanded = true;
    }
}

/// Degraded evidence is counted, never rounded up to current coverage.
fn count_evidence(coverage: &mut ImpactCoverage, relation: &RelationResult) {
    match relation.support {
        Support::Supported => {}
        Support::Partial | Support::Unsupported => coverage.partial_support += 1,
    }
    match relation.freshness {
        Freshness::Fresh => {}
        Freshness::Dirty => coverage.dirty_evidence += 1,
        Freshness::Stale => coverage.stale_evidence += 1,
    }
}

/// Task 8's gap axes, carried forward unchanged. A gap never becomes an
/// impact edge.
fn count_gaps(coverage: &mut ImpactCoverage, found: &[AttributedGap]) {
    for attributed in found {
        coverage.gaps += 1;
        if !attributed.gap.candidates.is_empty() {
            coverage.ambiguous += 1;
        }
        if attributed.gap.reason.requires_semantics() {
            coverage.requires_semantics += 1;
        }
        if attributed.gap.reason.is_unsupported_construct() {
            coverage.unsupported_construct += 1;
        }
        if attributed.gap.candidate_truncated {
            coverage.candidate_truncated += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use brainprint_core::{ResourceId, SymbolId};
    use rusqlite::params;

    use super::*;
    use crate::{
        config::WorkspaceConfig,
        evidence::{OccurrenceRef, RelationEvidence, replace_resource_graph},
        gaps::{IntendedRelation, MAX_CANDIDATES, UnresolvedEvidence, UnresolvedReason},
        generation,
        graph::{GraphStore, Relation},
        resolution::{Dispatch, EvidenceBasis},
        resource::ResourceStore,
        scan::BaselineScan,
        structural::{self, StructuralState},
        symbol::{OccurrenceKind, SymbolStore},
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    /// `f` calls `g`, and `g` calls `f`: a CALLS cycle, and an IMPORTS
    /// cycle around it.
    const A_TS: &str = "\
import { g } from './b'

export function f(): number {
  return g()
}
";

    const B_TS: &str = "\
import { f } from './a'

export function g(): number {
  return f()
}
";

    const BASE_TS: &str = "export class Base {}\n";

    /// Calls `f`, references `f` as a value, extends `Base`, and names
    /// `Base` as a type.
    const C_TS: &str = "\
import { f } from './a'
import { Base } from './base'

export class Child extends Base {
  go(input: Base): number {
    register(f)
    return f()
  }
}
";

    /// Imports `c`, and states every gap shape.
    const D_TS: &str = "\
import { Child } from './c'
import { missing } from './gone'

export function make(input: Child, other: Base): number {
  obj.foo()
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
                "brainprint-impact-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            fs::create_dir_all(root.join("src")).expect("src");
            let fixture = Self { base, root };
            fixture.write("src/a.ts", A_TS);
            fixture.write("src/b.ts", B_TS);
            fixture.write("src/base.ts", BASE_TS);
            fixture.write("src/c.ts", C_TS);
            fixture.write("src/d.ts", D_TS);
            let many: String = (0..MAX_CANDIDATES + 5)
                .map(|index| {
                    format!("export function m{index}(): number {{\n  return {index}\n}}\n")
                })
                .collect();
            fixture.write("src/many.ts", &many);
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

        fn many_candidates(&self) -> Vec<GraphEndpoint> {
            SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_for_resource(self.resource("src/many.ts").id)
                .expect("symbols")
                .into_iter()
                .map(|symbol| GraphEndpoint::Symbol(symbol.id))
                .collect()
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

        /// Import Occurrences that are the module specifier, not the
        /// imported name.
        fn specifiers(&self, rel: &str) -> Vec<OccurrenceRef> {
            let source = fs::read_to_string(self.root.join(rel)).expect("source");
            self.sites(rel, OccurrenceKind::ImportSite)
                .into_iter()
                .filter(|site| source[site.start_byte..site.end_byte].starts_with('\''))
                .collect()
        }

        /// Import Occurrences that are the imported name.
        fn import_names(&self, rel: &str) -> Vec<OccurrenceRef> {
            let source = fs::read_to_string(self.root.join(rel)).expect("source");
            self.sites(rel, OccurrenceKind::ImportSite)
                .into_iter()
                .filter(|site| !source[site.start_byte..site.end_byte].starts_with('\''))
                .collect()
        }

        fn traversal(&self) -> ImpactTraversal {
            ImpactTraversal::open(&self.db_path()).expect("index.db")
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

        fn publish_baseline(&self) {
            let a = self.file("src/a.ts");
            let b = self.file("src/b.ts");
            let c = self.file("src/c.ts");
            let d = self.file("src/d.ts");
            let base_file = self.file("src/base.ts");
            let f = self.declaration("src/a.ts", "f");
            let g = self.declaration("src/b.ts", "g");
            let base = self.declaration("src/base.ts", "Base");
            let child = self.declaration("src/c.ts", "Child");
            let go = self.declaration("src/c.ts", "Child.go");

            let a_calls = self.sites("src/a.ts", OccurrenceKind::CallSite);
            let b_calls = self.sites("src/b.ts", OccurrenceKind::CallSite);
            let c_calls = self.sites("src/c.ts", OccurrenceKind::CallSite);
            let c_refs = self.sites("src/c.ts", OccurrenceKind::ReferenceSite);
            let c_types = self.sites("src/c.ts", OccurrenceKind::TypeSite);
            let d_types = self.sites("src/d.ts", OccurrenceKind::TypeSite);
            let candidates = self.many_candidates();

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

            let plan = vec![
                (
                    "src/a.ts",
                    vec![
                        evidence(a_calls[0], edge(RelationKind::Calls, &f, &g, published)),
                        evidence(
                            self.specifiers("src/a.ts")[0],
                            edge(RelationKind::Imports, &a, &b, published),
                        ),
                    ],
                    Vec::new(),
                ),
                (
                    "src/b.ts",
                    vec![
                        evidence(b_calls[0], edge(RelationKind::Calls, &g, &f, published)),
                        evidence(
                            self.specifiers("src/b.ts")[0],
                            edge(RelationKind::Imports, &b, &a, published),
                        ),
                    ],
                    Vec::new(),
                ),
                (
                    "src/c.ts",
                    vec![
                        evidence(c_calls[1], edge(RelationKind::Calls, &go, &f, published)),
                        evidence(
                            c_refs[0],
                            edge(RelationKind::References, &go, &f, published),
                        ),
                        evidence(
                            c_types[0],
                            edge(RelationKind::Extends, &child, &base, published),
                        ),
                        evidence(
                            c_types[1],
                            edge(RelationKind::UsesType, &go, &base, published),
                        ),
                        evidence(
                            self.specifiers("src/c.ts")[0],
                            edge(RelationKind::Imports, &c, &a, published),
                        ),
                        evidence(
                            self.specifiers("src/c.ts")[1],
                            edge(RelationKind::Imports, &c, &base_file, published),
                        ),
                    ],
                    Vec::new(),
                ),
                (
                    "src/d.ts",
                    vec![evidence(
                        self.specifiers("src/d.ts")[0],
                        edge(RelationKind::Imports, &d, &c, published),
                    )],
                    vec![
                        // Ambiguous: candidates in hand, and they stay
                        // candidates.
                        UnresolvedEvidence {
                            occurrence: d_types[0],
                            intended: IntendedRelation::Known(RelationKind::UsesType),
                            lookup_name: "Child".to_owned(),
                            module_hint: None,
                            reason: UnresolvedReason::AmbiguousCandidates,
                            candidates: vec![base.clone(), child.clone()],
                        },
                        // Semantic-required, attributable to Base.
                        UnresolvedEvidence {
                            occurrence: d_types[1],
                            intended: IntendedRelation::Known(RelationKind::UsesType),
                            lookup_name: "Base".to_owned(),
                            module_hint: None,
                            reason: UnresolvedReason::TypeSemanticsRequired,
                            candidates: vec![base.clone()],
                        },
                        // Unsupported construct, attributable to c.ts.
                        UnresolvedEvidence {
                            occurrence: self.import_names("src/d.ts")[0],
                            intended: IntendedRelation::Known(RelationKind::Imports),
                            lookup_name: "./c".to_owned(),
                            module_hint: None,
                            reason: UnresolvedReason::CompoundSpecifier,
                            candidates: vec![c.clone()],
                        },
                        // More candidates than the limit keeps.
                        UnresolvedEvidence {
                            occurrence: self.specifiers("src/d.ts")[1],
                            intended: IntendedRelation::Known(RelationKind::UsesType),
                            lookup_name: "Overloaded".to_owned(),
                            module_hint: None,
                            reason: UnresolvedReason::AmbiguousCandidates,
                            candidates: candidates.clone(),
                        },
                        // No target at all: counted, never attached.
                        UnresolvedEvidence {
                            occurrence: self.sites("src/d.ts", OccurrenceKind::CallSite)[0],
                            intended: IntendedRelation::Known(RelationKind::Calls),
                            lookup_name: "foo".to_owned(),
                            module_hint: None,
                            reason: UnresolvedReason::ReceiverTypeRequired,
                            candidates: Vec::new(),
                        },
                    ],
                ),
            ];

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
            time_limit: Duration::from_secs(30),
        }
    }

    fn kinds_of(result: &ImpactResult) -> Vec<RelationKind> {
        let mut kinds: Vec<RelationKind> =
            result.edges.iter().map(|edge| edge.relation.kind).collect();
        kinds.sort_by_key(|kind| kind.as_str());
        kinds.dedup();
        kinds
    }

    fn endpoints_of(result: &ImpactResult) -> Vec<GraphEndpoint> {
        result
            .nodes
            .iter()
            .map(|node| node.endpoint.clone())
            .collect()
    }

    fn edge_keys(result: &ImpactResult) -> Vec<(RelationKind, GraphEndpoint, GraphEndpoint)> {
        result
            .edges
            .iter()
            .map(|edge| {
                (
                    edge.relation.kind,
                    edge.relation.source.clone(),
                    edge.relation.target.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn a_calls_cycle_terminates_and_repeats_no_node_or_edge() {
        let fixture = Fixture::create("cycle");
        fixture.publish_baseline();
        let f = fixture.declaration("src/a.ts", "f");

        let result = fixture
            .traversal()
            .run(ImpactIntent::PublicSignatureChange, &f, &generous())
            .expect("traverse");

        // f <- g <- f would never end without a cycle guard.
        assert!(result.is_complete_walk(), "{:?}", result.truncation);
        let mut endpoints = endpoints_of(&result);
        let before = endpoints.len();
        endpoints.sort_by_key(graph::endpoint_sort_key);
        endpoints.dedup();
        assert_eq!(endpoints.len(), before, "an endpoint is emitted once");

        let mut keys = edge_keys(&result);
        let edges_before = keys.len();
        keys.sort_by(|left, right| edge_order(left).cmp(&edge_order(right)));
        keys.dedup();
        assert_eq!(keys.len(), edges_before, "a canonical edge is emitted once");

        assert_eq!(
            result.budget.expanded,
            result.nodes.iter().filter(|node| node.expanded).count()
        );
        assert!(
            result.nodes.iter().all(|node| node.expanded),
            "every reached endpoint was expanded exactly once"
        );
    }

    #[test]
    fn an_endpoint_reached_by_two_paths_is_expanded_once() {
        let fixture = Fixture::create("two-paths");
        fixture.publish_baseline();
        let f = fixture.declaration("src/a.ts", "f");
        let go = fixture.declaration("src/c.ts", "Child.go");

        // Child.go both calls and references f: two edges, one endpoint.
        let result = fixture
            .traversal()
            .run(ImpactIntent::Rename, &f, &generous())
            .expect("traverse");

        assert_eq!(
            endpoints_of(&result)
                .iter()
                .filter(|endpoint| **endpoint == go)
                .count(),
            1,
            "reached twice, expanded and emitted once"
        );
        let into_go: Vec<RelationKind> = result
            .edges
            .iter()
            .filter(|edge| edge.relation.source == go && edge.relation.target == f)
            .map(|edge| edge.relation.kind)
            .collect();
        assert_eq!(into_go.len(), 2, "both edges are kept: {into_go:?}");
    }

    #[test]
    fn each_intent_follows_only_its_own_relation_kinds() {
        let fixture = Fixture::create("plans");
        fixture.publish_baseline();
        let traversal = fixture.traversal();
        let f = fixture.declaration("src/a.ts", "f");
        let base = fixture.declaration("src/base.ts", "Base");
        let a = fixture.file("src/a.ts");

        // A signature change does not follow REFERENCES, though one
        // exists into f.
        let signature = traversal
            .run(ImpactIntent::PublicSignatureChange, &f, &generous())
            .expect("traverse");
        assert_eq!(kinds_of(&signature), vec![RelationKind::Calls]);
        assert!(
            !signature
                .edges
                .iter()
                .any(|edge| edge.relation.kind == RelationKind::References)
        );

        // A rename does.
        let rename = traversal
            .run(ImpactIntent::Rename, &f, &generous())
            .expect("traverse");
        assert_eq!(
            kinds_of(&rename),
            vec![RelationKind::Calls, RelationKind::References]
        );
        assert!(
            rename
                .plan
                .iter()
                .all(|step| step.direction == Direction::Incoming),
            "impact flows one way"
        );

        // A module move follows IMPORTS, not the calls between the same
        // files.
        let moved = traversal
            .run(ImpactIntent::ModuleMove, &a, &generous())
            .expect("traverse");
        assert_eq!(kinds_of(&moved), vec![RelationKind::Imports]);
        assert!(endpoints_of(&moved).contains(&fixture.file("src/c.ts")));
        assert!(
            endpoints_of(&moved).contains(&fixture.file("src/d.ts")),
            "d.ts imports c.ts, which imports a.ts"
        );
        assert!(
            !endpoints_of(&moved).contains(&f),
            "a file move is not a call"
        );

        // A base change follows the hierarchy and type uses.
        let base_change = traversal
            .run(ImpactIntent::BaseInterfaceChange, &base, &generous())
            .expect("traverse");
        assert_eq!(
            kinds_of(&base_change),
            vec![RelationKind::Extends, RelationKind::UsesType]
        );
        assert!(endpoints_of(&base_change).contains(&fixture.declaration("src/c.ts", "Child")));
        assert!(endpoints_of(&base_change).contains(&fixture.declaration("src/c.ts", "Child.go")));
    }

    #[test]
    fn the_node_budget_truncates_explicitly() {
        let fixture = Fixture::create("node-budget");
        fixture.publish_baseline();
        let f = fixture.declaration("src/a.ts", "f");

        let result = fixture
            .traversal()
            .run(
                ImpactIntent::Rename,
                &f,
                &Budget {
                    max_nodes: 2,
                    ..generous()
                },
            )
            .expect("traverse");

        assert_eq!(result.truncation, Some(Truncation::NodeBudget));
        assert_eq!(result.nodes.len(), 2, "the root and one more");
        assert!(!result.coverage.is_complete(), "a budget is not an answer");
        assert!(result.continuation.is_some());
    }

    #[test]
    fn the_edge_budget_truncates_explicitly() {
        let fixture = Fixture::create("edge-budget");
        fixture.publish_baseline();
        let f = fixture.declaration("src/a.ts", "f");

        let result = fixture
            .traversal()
            .run(
                ImpactIntent::Rename,
                &f,
                &Budget {
                    max_edges: 1,
                    ..generous()
                },
            )
            .expect("traverse");

        assert_eq!(result.truncation, Some(Truncation::EdgeBudget));
        assert_eq!(result.edges.len(), 1);
        assert!(!result.coverage.is_complete());
        assert!(result.continuation.is_some());
    }

    #[test]
    fn the_depth_budget_truncates_explicitly() {
        let fixture = Fixture::create("depth-budget");
        fixture.publish_baseline();
        let a = fixture.file("src/a.ts");

        let result = fixture
            .traversal()
            .run(
                ImpactIntent::ModuleMove,
                &a,
                &Budget {
                    max_depth: 1,
                    ..generous()
                },
            )
            .expect("traverse");

        assert_eq!(result.truncation, Some(Truncation::DepthBudget));
        assert!(
            result.nodes.iter().all(|node| node.depth <= 1),
            "nothing beyond the budgeted depth"
        );
        assert!(
            result
                .nodes
                .iter()
                .any(|node| node.depth == 1 && !node.expanded),
            "reached but deliberately not expanded"
        );
        assert!(!endpoints_of(&result).contains(&fixture.file("src/d.ts")));
        assert!(!result.coverage.is_complete());
    }

    #[test]
    fn the_time_budget_truncates_explicitly() {
        let fixture = Fixture::create("time-budget");
        fixture.publish_baseline();
        let f = fixture.declaration("src/a.ts", "f");

        let result = fixture
            .traversal()
            .run(
                ImpactIntent::Rename,
                &f,
                &Budget {
                    time_limit: Duration::ZERO,
                    ..generous()
                },
            )
            .expect("traverse");

        assert_eq!(result.truncation, Some(Truncation::TimeBudget));
        assert!(result.edges.is_empty(), "nothing was expanded");
        assert!(!result.coverage.is_complete());
        let continuation = result.continuation.expect("resumable");
        assert_eq!(continuation.pending(), 1, "the root is still to expand");
    }

    #[test]
    fn a_continuation_resumes_without_re_emitting_anything() {
        let fixture = Fixture::create("continuation");
        fixture.publish_baseline();
        let traversal = fixture.traversal();
        let f = fixture.declaration("src/a.ts", "f");

        let whole = traversal
            .run(ImpactIntent::Rename, &f, &generous())
            .expect("traverse");
        assert!(whole.is_complete_walk());

        let first = traversal
            .run(
                ImpactIntent::Rename,
                &f,
                &Budget {
                    max_nodes: 2,
                    ..generous()
                },
            )
            .expect("traverse");
        let continuation = first.continuation.clone().expect("truncated");

        let second = traversal
            .resume(ImpactIntent::Rename, &f, &continuation, &generous())
            .expect("resume");

        // Nothing comes back twice.
        for endpoint in endpoints_of(&second) {
            assert!(
                !endpoints_of(&first).contains(&endpoint),
                "{endpoint:?} was already returned"
            );
        }
        for key in edge_keys(&second) {
            assert!(
                !edge_keys(&first).contains(&key),
                "{key:?} was already returned"
            );
        }

        // And together they are the whole walk.
        let mut resumed = endpoints_of(&first);
        resumed.extend(endpoints_of(&second));
        resumed.sort_by_key(graph::endpoint_sort_key);
        let mut expected = endpoints_of(&whole);
        expected.sort_by_key(graph::endpoint_sort_key);
        assert_eq!(resumed, expected);

        let mut resumed_edges = edge_keys(&first);
        resumed_edges.extend(edge_keys(&second));
        resumed_edges.sort_by(|left, right| edge_order(left).cmp(&edge_order(right)));
        let mut expected_edges = edge_keys(&whole);
        expected_edges.sort_by(|left, right| edge_order(left).cmp(&edge_order(right)));
        assert_eq!(resumed_edges, expected_edges);
        assert!(second.is_complete_walk(), "the rest fitted this time");
    }

    #[test]
    fn a_continuation_is_refused_by_another_root_or_intent() {
        let fixture = Fixture::create("continuation-binding");
        fixture.publish_baseline();
        let traversal = fixture.traversal();
        let f = fixture.declaration("src/a.ts", "f");
        let base = fixture.declaration("src/base.ts", "Base");

        let truncated = traversal
            .run(
                ImpactIntent::Rename,
                &f,
                &Budget {
                    max_nodes: 2,
                    ..generous()
                },
            )
            .expect("traverse");
        let continuation = truncated.continuation.expect("truncated");

        assert!(matches!(
            traversal.resume(ImpactIntent::Rename, &base, &continuation, &generous()),
            Err(ImpactError::ContinuationMismatch { field: "root", .. })
        ));
        assert!(matches!(
            traversal.resume(
                ImpactIntent::PublicSignatureChange,
                &f,
                &continuation,
                &generous()
            ),
            Err(ImpactError::ContinuationMismatch {
                field: "intent",
                ..
            })
        ));
    }

    #[test]
    fn gaps_propagate_without_becoming_impact_edges() {
        let fixture = Fixture::create("gaps");
        fixture.publish_baseline();
        let base = fixture.declaration("src/base.ts", "Base");

        let result = fixture
            .traversal()
            .run(ImpactIntent::BaseInterfaceChange, &base, &generous())
            .expect("traverse");

        // d.ts states an ambiguous use and a semantics-required use,
        // both naming Base as a candidate.
        assert_eq!(result.coverage.gaps, 2, "{:?}", result.gaps);
        assert_eq!(result.coverage.ambiguous, 2);
        assert_eq!(result.coverage.requires_semantics, 1);
        assert!(result.gaps.iter().all(|gap| gap.at == base));
        assert!(
            result
                .gaps
                .iter()
                .all(|gap| gap.gap.resolution == crate::resolution::Resolution::Candidate),
            "a candidate stays a candidate"
        );
        assert!(
            !result
                .edges
                .iter()
                .any(|edge| edge.relation.source == fixture.file("src/d.ts")),
            "an unresolved use site is not an impact edge"
        );
        assert!(result.coverage.unattributed > 0, "counted, never attached");
        assert!(!result.coverage.is_complete());
        assert!(
            result.is_complete_walk(),
            "the walk finished; the coverage is what is incomplete"
        );
    }

    #[test]
    fn an_unsupported_gap_propagates_through_a_module_move() {
        let fixture = Fixture::create("unsupported-gap");
        fixture.publish_baseline();
        let a = fixture.file("src/a.ts");

        let result = fixture
            .traversal()
            .run(ImpactIntent::ModuleMove, &a, &generous())
            .expect("traverse");

        assert_eq!(result.coverage.unsupported_construct, 1);
        assert!(
            result
                .gaps
                .iter()
                .any(|gap| gap.gap.reason == UnresolvedReason::CompoundSpecifier
                    && gap.at == fixture.file("src/c.ts")),
            "attributed to the endpoint whose candidate it names"
        );
        assert!(!result.coverage.is_complete());
    }

    #[test]
    fn candidate_truncation_and_traversal_truncation_are_separate_facts() {
        let fixture = Fixture::create("two-truncations");
        fixture.publish_baseline();
        let traversal = fixture.traversal();

        // A gap whose candidate list was cut, attributed to a candidate
        // that survived the cut.
        let survivor = UnresolvedEvidence {
            occurrence: fixture.sites("src/d.ts", OccurrenceKind::TypeSite)[0],
            intended: IntendedRelation::Known(RelationKind::UsesType),
            lookup_name: "Overloaded".to_owned(),
            module_hint: None,
            reason: UnresolvedReason::AmbiguousCandidates,
            candidates: fixture.many_candidates(),
        }
        .bounded_candidates()
        .0[0]
            .clone();

        let candidate_only = traversal
            .run(ImpactIntent::BaseInterfaceChange, &survivor, &generous())
            .expect("traverse");
        assert_eq!(candidate_only.coverage.candidate_truncated, 1);
        assert!(
            candidate_only.is_complete_walk(),
            "the candidate list was cut, not the walk"
        );
        assert_eq!(candidate_only.truncation, None);

        // A walk cut by its own budget, with no candidate truncation.
        let walk_only = traversal
            .run(
                ImpactIntent::Rename,
                &fixture.declaration("src/a.ts", "f"),
                &Budget {
                    max_nodes: 2,
                    ..generous()
                },
            )
            .expect("traverse");
        assert_eq!(walk_only.truncation, Some(Truncation::NodeBudget));
        assert_eq!(walk_only.coverage.candidate_truncated, 0);
    }

    #[test]
    fn stale_and_partial_evidence_prevent_a_complete_claim() {
        let fixture = Fixture::create("degraded");
        fixture.publish_baseline();
        let f = fixture.declaration("src/a.ts", "f");
        let c = fixture.resource("src/c.ts").id;

        let clean = fixture
            .traversal()
            .run(ImpactIntent::PublicSignatureChange, &f, &generous())
            .expect("traverse");
        assert_eq!(clean.coverage.stale_evidence, 0);
        assert_eq!(clean.coverage.partial_support, 0);

        // c.ts stops parsing cleanly, and b.ts moves on.
        let store = GraphStore::open(&fixture.db_path()).expect("index.db");
        structural::write(
            store.connection(),
            c,
            StructuralState::Partial,
            "workspace-rev-1",
            Some(1),
            None,
        )
        .expect("structural state");
        store
            .connection()
            .execute(
                "UPDATE resource SET resource_revision = 'moved' WHERE uid = ?1",
                params![fixture.resource("src/b.ts").id.to_bytes().to_vec()],
            )
            .expect("bump revision");
        drop(store);

        let degraded = fixture
            .traversal()
            .run(ImpactIntent::PublicSignatureChange, &f, &generous())
            .expect("traverse");
        assert!(degraded.coverage.stale_evidence > 0, "b.ts moved on");
        assert!(degraded.coverage.partial_support > 0, "c.ts is partial");
        assert!(degraded.is_complete_walk(), "the walk itself finished");
        assert!(
            !degraded.coverage.is_complete(),
            "degraded evidence is not current coverage"
        );
    }

    #[test]
    fn identical_state_produces_identical_output() {
        let fixture = Fixture::create("deterministic");
        fixture.publish_baseline();
        let traversal = fixture.traversal();
        let base = fixture.declaration("src/base.ts", "Base");

        let first = traversal
            .run(ImpactIntent::BaseInterfaceChange, &base, &generous())
            .expect("traverse");
        let second = traversal
            .run(ImpactIntent::BaseInterfaceChange, &base, &generous())
            .expect("traverse");
        assert_eq!(first, second);

        let cut = Budget {
            max_nodes: 2,
            ..generous()
        };
        let left = traversal
            .run(
                ImpactIntent::Rename,
                &fixture.declaration("src/a.ts", "f"),
                &cut,
            )
            .expect("traverse");
        let right = traversal
            .run(
                ImpactIntent::Rename,
                &fixture.declaration("src/a.ts", "f"),
                &cut,
            )
            .expect("traverse");
        assert_eq!(
            left.continuation, right.continuation,
            "the continuation boundary is deterministic"
        );
    }

    #[test]
    fn traversal_stores_nothing_and_leaks_no_storage_ids() {
        let fixture = Fixture::create("read-only");
        fixture.publish_baseline();
        let f = fixture.declaration("src/a.ts", "f");

        let before = stored_kinds(&fixture);
        let result = fixture
            .traversal()
            .run(ImpactIntent::Rename, &f, &generous())
            .expect("traverse");
        let after = stored_kinds(&fixture);

        assert_eq!(before, after, "a query writes nothing");
        assert!(
            !after.iter().any(|kind| kind.ends_with("_BY")),
            "no reverse relation row: {after:?}"
        );

        // The result speaks only in stable identity.
        let known: Vec<GraphEndpoint> = vec![
            f.clone(),
            fixture.declaration("src/b.ts", "g"),
            fixture.declaration("src/c.ts", "Child.go"),
        ];
        for node in &result.nodes {
            assert!(
                known.contains(&node.endpoint),
                "unexpected endpoint {:?}",
                node.endpoint
            );
            assert!(matches!(
                node.endpoint,
                GraphEndpoint::Symbol(_) | GraphEndpoint::Resource(_)
            ));
        }
        for edge in &result.edges {
            for location in &edge.relation.evidence {
                assert!(
                    [
                        fixture.resource("src/a.ts").id,
                        fixture.resource("src/b.ts").id,
                        fixture.resource("src/c.ts").id,
                    ]
                    .contains(&location.resource)
                );
            }
        }
        let _: Vec<ResourceId> = Vec::new();
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
