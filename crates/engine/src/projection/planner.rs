//! Read-only deterministic projection planner (#20 task 6).
//!
//! Turns one validated [`ProjectionRequest`] into a [`PreparedProjection`]
//! from current persisted truth, by composing the existing query surfaces:
//! `QueryIndex` for target selection, `RelationIndex` for direct edges,
//! `ImpactTraversal` + `RelatedTests::from_impact` for structural change,
//! the task 2 resolver for knowledge, `WorkRuntime` for Working State, and
//! `SourceReader` for the few ranges that must be read.
//!
//! What it never does: write any store, refresh anything, or reach a
//! semantic backend -- persisted coverage (`RequiresSemantics`, stale,
//! dirty, unsupported) is returned as corrective evidence instead. It
//! decides *what is relevant*; how much of it is delivered is task 7's.
//!
//! Output order is fixed by category, then by canonical identity/locator
//! where one exists, else by the (already deterministic) order of the
//! surface that produced it:
//!
//! 1. target identity / selection
//! 2. required target source
//! 3. rules (Policy, request directive)
//! 4. Decision / Preference / Project State / Blueprint
//! 5. relations and relation gaps
//! 6. related tests
//! 7. Working State
//! 8. corrective (conflict, source unavailable, coverage, currentness)

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    path::Path,
};

use brainprint_core::{BlueprintApplicationId, ProjectId, ResourceId, WorkItemId, WorkspaceId};

use super::{
    ChangeKind, CoverageEvidence, CoverageSubject, EvidenceItem, GenerationBasis, ProjectionIntent,
    ProjectionKnowledgeRefs, ProjectionRequest, ProjectionRequestError, ProjectionTarget,
    TargetSelection,
    canonical::{self, Canonical},
};
use crate::{
    coverage::{CoverageLimit, CoverageReport},
    generation::{GenerationError, GenerationStore},
    graph::{self, GraphEndpoint},
    impact::{Budget, ImpactError, ImpactIntent},
    inspect::{ReadError, SourceReader},
    knowledge::{
        ApplicabilityContext, GlobalKnowledgeStore, KnowledgeError, KnowledgeSources,
        ProjectKnowledgeStore, RequestDirective, ResolveError, ResolveRequest, ResolvedKnowledge,
        Staleness, WorkError, WorkRuntime, WorkspaceKnowledgeStore, resolve,
    },
    logical_symbol::{self, LogicalSymbolError},
    parser::SourceSpan,
    paths::WorkspacePaths,
    prepare::{PrepareError, PreparedRange, RangeRole, unavailable_from},
    query::{
        Located, QueryError, QueryIndex, ResourceLocator, StructuralCoverage, SymbolCandidate,
        SymbolQuery, SymbolSelector,
    },
    registry::{GlobalRegistry, RegistryError},
    related_tests::{RelatedTestError, RelatedTests},
    relations::{Direction, RelationError},
};

// ---------------------------------------------------------------- output

/// What the planner could not deterministically establish. A fact, never
/// advice: it is what a later guard reads to allow a native fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionGap {
    /// Several candidates matched; none was chosen.
    TargetAmbiguous,
    /// A search selector matched once; a search result is not an identity.
    TargetNotExact,
    /// No candidate, and the coverage behind that is complete.
    TargetNotFound,
    /// No candidate, but coverage/currentness does not allow "none".
    TargetNotFoundWithIncompleteCoverage,
    /// An exact local identity that is not current in the index.
    TargetNotCurrent,
    /// No canonical transitive plan exists for this change form; any
    /// relations returned are direct only.
    UnsupportedImpactProfile(ChangeKind),
    /// A plain edit: no dependency expansion is defined for it.
    DependencyExpansionUndefined,
    /// A named Blueprint Application is not ACTIVE and applicable here.
    BlueprintApplicationNotApplied(BlueprintApplicationId),
    /// Persisted coverage says a semantic answer is needed and absent.
    RequiresSemantics,
    /// The structural index is not current.
    NotCurrent,
}

/// Whether a planned range must be read now or is a candidate for later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceRequirement {
    /// The exact target declaration an UNDERSTAND/CHANGE needs.
    Required,
    /// Evidence a delivery budget may choose to read (task 7).
    Optional,
}

/// A source range the planner knows about, without its body. Engine
/// internal and never persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedSourceRange {
    pub resource: ResourceId,
    pub resource_revision: String,
    pub span: SourceSpan,
    pub role: RangeRole,
    pub requirement: SourceRequirement,
}

/// Which delivery tier an evidence item belongs to (#20 task 7 §7).
/// Selection metadata only: never a fact, never persisted, never a score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relevance {
    /// Target identity, selection, or its required source.
    Target,
    /// A relation found at the target itself.
    DirectRelation,
    /// A relation the I3 traversal found beyond the target.
    TransitiveImpact,
    /// Policy or a request directive.
    ApplicableRule,
    /// An exactly requested Decision / Preference / State / Blueprint.
    RequestedKnowledge,
    RelatedTest,
    /// A relation gap: detail behind a coverage limit.
    CoverageSupport,
    WorkingState,
    /// Conflict, source unavailable, coverage, currentness.
    Corrective,
}

/// The planner's delivery sidecar for one evidence item, in the same
/// position as the item. `impact_depth` is the I3 `ImpactEdge.depth`
/// (0 = found at the root), kept because flattening loses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryHint {
    pub relevance: Relevance,
    pub impact_depth: Option<usize>,
}

/// One planned projection. Engine-internal; not a transport shape.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedProjection {
    pub workspace: WorkspaceId,
    pub project: ProjectId,
    pub intent: ProjectionIntent,
    /// The exact target, when selection produced one.
    pub target: Option<GraphEndpoint>,
    /// Deduplicated, in the module's category order.
    pub evidence: Vec<EvidenceItem>,
    pub gaps: Vec<ProjectionGap>,
    /// Required ranges (already read, merged) then optional candidates.
    pub source_plan: Vec<PlannedSourceRange>,
    /// One hint per `evidence` item, same order.
    pub delivery: Vec<DeliveryHint>,
    /// What the query surfaces actually returned before preparation
    /// (#21): counted where they returned it, never by a second query.
    pub raw: StageAmount,
}

/// What the planner actually did, counted as it happened. Observations,
/// not savings; task 8 owns delivery accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlannerStats {
    pub plans: u64,
    pub source_file_reads: u64,
    pub source_bytes: u64,
    pub relation_queries: u64,
    pub impact_traversals: u64,
    pub knowledge_resolves: u64,
    pub knowledge_items: u64,
    pub work_snapshots: u64,
    /// Task 7: optional source ranges a page could have read, and the
    /// ones its budget selected for reading.
    pub optional_source_candidates: u64,
    pub optional_source_selected: u64,
}

// ----------------------------------------------------------------- error

#[derive(Debug)]
pub enum PlannerError {
    Request(ProjectionRequestError),
    /// The request names another Workspace than the planner is bound to.
    WorkspaceMismatch {
        bound: WorkspaceId,
        requested: WorkspaceId,
    },
    MissingGlobalDb,
    /// NOT_INITIALIZED: no registry entry for the Workspace.
    UnknownWorkspace(WorkspaceId),
    MissingWorkspaceRoot,
    Registry(RegistryError),
    Knowledge(KnowledgeError),
    Work(WorkError),
    Query(QueryError),
    Read(ReadError),
    Relation(RelationError),
    Impact(ImpactError),
    RelatedTests(RelatedTestError),
    LogicalSymbol(LogicalSymbolError),
    Resolve(ResolveError),
    Prepare(PrepareError),
    Generation(GenerationError),
}

impl fmt::Display for PlannerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(source) => write!(formatter, "invalid projection request: {source}"),
            Self::WorkspaceMismatch { bound, requested } => write!(
                formatter,
                "planner is bound to Workspace {bound}, request names {requested}"
            ),
            Self::MissingGlobalDb => formatter.write_str("global.db does not exist"),
            Self::UnknownWorkspace(id) => write!(formatter, "Workspace {id} is not initialized"),
            Self::MissingWorkspaceRoot => formatter.write_str("the Workspace root does not exist"),
            Self::Registry(source) => write!(formatter, "{source}"),
            Self::Knowledge(source) => write!(formatter, "{source}"),
            Self::Work(source) => write!(formatter, "{source}"),
            Self::Query(source) => write!(formatter, "{source}"),
            Self::Read(source) => write!(formatter, "{source}"),
            Self::Relation(source) => write!(formatter, "{source}"),
            Self::Impact(source) => write!(formatter, "{source}"),
            Self::RelatedTests(source) => write!(formatter, "{source}"),
            Self::LogicalSymbol(source) => write!(formatter, "{source}"),
            Self::Resolve(source) => write!(formatter, "{source}"),
            Self::Prepare(source) => write!(formatter, "{source}"),
            Self::Generation(source) => write!(formatter, "{source}"),
        }
    }
}

impl Error for PlannerError {}

macro_rules! from_error {
    ($($variant:ident($source:ty)),* $(,)?) => {
        $(impl From<$source> for PlannerError {
            fn from(source: $source) -> Self {
                Self::$variant(source)
            }
        })*
    };
}

from_error!(
    Request(ProjectionRequestError),
    Registry(RegistryError),
    Knowledge(KnowledgeError),
    Work(WorkError),
    Query(QueryError),
    Read(ReadError),
    Relation(RelationError),
    Impact(ImpactError),
    RelatedTests(RelatedTestError),
    LogicalSymbol(LogicalSymbolError),
    Resolve(ResolveError),
    Prepare(PrepareError),
    Generation(GenerationError),
);

// --------------------------------------------------------------- planner

/// The planner for one Workspace, bound through the registry.
pub struct ProjectionPlanner {
    project_id: ProjectId,
    workspace_id: WorkspaceId,
    global: GlobalKnowledgeStore,
    project: ProjectKnowledgeStore,
    workspace: WorkspaceKnowledgeStore,
    work: WorkRuntime,
    reader: SourceReader,
    tests: RelatedTests,
    generations: GenerationStore,
    stats: Cell<PlannerStats>,
}

impl ProjectionPlanner {
    /// Bind `workspace_id` via the registry in `global_db`: its Project,
    /// root, project-home project.db, workspace.db and index.db must all
    /// exist and be bound to exactly these identities. Nothing is created,
    /// bound, or repaired.
    pub fn open(global_db: &Path, workspace_id: WorkspaceId) -> Result<Self, PlannerError> {
        if !global_db.is_file() {
            return Err(PlannerError::MissingGlobalDb);
        }
        let registry = GlobalRegistry::open(global_db)?;
        let entry = registry
            .get_workspace(workspace_id)?
            .ok_or(PlannerError::UnknownWorkspace(workspace_id))?;
        if !entry.locator.is_dir() {
            return Err(PlannerError::MissingWorkspaceRoot);
        }
        let paths = WorkspacePaths::from_root(&entry.locator);
        // Checks both files exist and are bound to this Workspace before
        // anything else opens (and could create) them.
        let work = WorkRuntime::open(workspace_id, &paths.workspace_db, &paths.index_db)?;
        let project = ProjectKnowledgeStore::open_project_home(&registry, entry.project_id)?;
        Ok(Self {
            project_id: entry.project_id,
            workspace_id,
            global: GlobalKnowledgeStore::open(global_db)?,
            project,
            workspace: WorkspaceKnowledgeStore::open(&paths.workspace_db)?,
            work,
            reader: SourceReader::open(&paths.index_db, &entry.locator)?,
            tests: RelatedTests::open(&paths.index_db)?,
            generations: GenerationStore::open(&paths.index_db)?,
            stats: Cell::new(PlannerStats::default()),
        })
    }

    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    #[must_use]
    pub fn stats(&self) -> PlannerStats {
        self.stats.get()
    }

    fn count(&self, update: impl FnOnce(&mut PlannerStats)) {
        let mut stats = self.stats.get();
        update(&mut stats);
        self.stats.set(stats);
    }

    const fn index(&self) -> &QueryIndex {
        self.reader.index()
    }

    /// Plan `request`. An invalid request fails before any query runs.
    pub fn plan(&self, request: &ProjectionRequest) -> Result<PreparedProjection, PlannerError> {
        let context = request.validate()?;
        if request.workspace != self.workspace_id {
            return Err(PlannerError::WorkspaceMismatch {
                bound: self.workspace_id,
                requested: request.workspace,
            });
        }
        self.count(|stats| stats.plans += 1);

        let mut plan = Plan::default();
        let currentness = self.index().currentness()?;
        let changes = matches!(
            request.intent,
            ProjectionIntent::Change(_) | ProjectionIntent::Impact(_)
        );
        if changes || !currentness.is_current() {
            plan.items.push(EvidenceItem::IndexCurrentness {
                workspace: self.workspace_id,
                currentness,
            });
        }
        if !currentness.is_current() {
            plan.gap(ProjectionGap::NotCurrent);
        }

        let selected = match &request.target {
            Some(target) => self.select(target, &mut plan)?,
            None => None,
        };

        match request.intent {
            ProjectionIntent::Locate => {}
            ProjectionIntent::Understand => {
                if let Some(selected) = &selected {
                    plan.require_declarations(selected);
                    self.direct(
                        &selected.endpoint,
                        &[Direction::Outgoing, Direction::Incoming],
                        &mut plan,
                    )?;
                }
            }
            ProjectionIntent::Change(kind) => {
                if let Some(selected) = &selected {
                    plan.require_declarations(selected);
                    self.change_graph(&selected.endpoint, kind, &mut plan)?;
                }
                self.knowledge(request, context, &mut plan)?;
                if let Some(work_item) = request.work_item {
                    self.work(work_item, &mut plan)?;
                }
            }
            ProjectionIntent::Impact(kind) => {
                if let Some(selected) = &selected {
                    self.change_graph(&selected.endpoint, Some(kind), &mut plan)?;
                }
            }
            ProjectionIntent::ResumeHandoff => {
                self.knowledge(request, context, &mut plan)?;
                if let Some(work_item) = request.work_item {
                    self.work(work_item, &mut plan)?;
                }
            }
        }

        self.materialize(&mut plan)?;
        Ok(plan.finish(
            self.workspace_id,
            self.project_id,
            request.intent,
            selected.map(|selected| selected.endpoint),
        ))
    }

    // ---- target -------------------------------------------------------

    /// Resolve the target to one exact current identity, or record why
    /// not and return `None` -- which stops every target-dependent step.
    fn select(
        &self,
        target: &ProjectionTarget,
        plan: &mut Plan,
    ) -> Result<Option<Selected>, PlannerError> {
        let symbol_query = |id| SymbolQuery::new(SymbolSelector::Id(id));
        match target {
            ProjectionTarget::Endpoint(endpoint) => match endpoint {
                GraphEndpoint::Resource(id) => {
                    let located = self.index().locate_resource(ResourceLocator::Id(*id))?;
                    self.exact_resource(target, &located, plan, true)
                }
                GraphEndpoint::Symbol(id) => {
                    let located = self.index().search_symbols(&symbol_query(*id))?;
                    self.exact_symbol(target, &located, plan, true)
                }
                GraphEndpoint::Logical(id) => {
                    let mut declarations = Vec::new();
                    let ids = logical_symbol::declarations(self.index().connection(), *id)?;
                    for declaration in &ids {
                        let located = self.index().search_symbols(&symbol_query(*declaration))?;
                        plan.raw.known(&located.candidates);
                        match located.exact() {
                            Some(candidate) => declarations.push(candidate.clone()),
                            None => plan.gap(ProjectionGap::TargetNotCurrent),
                        }
                    }
                    if ids.is_empty() {
                        plan.gap(ProjectionGap::TargetNotCurrent);
                        return Ok(None);
                    }
                    for declaration in &declarations {
                        plan.items.push(EvidenceItem::Symbol(declaration.clone()));
                    }
                    Ok(Some(Selected {
                        endpoint: endpoint.clone(),
                        declarations,
                    }))
                }
                // Canonical graph identities with no local source.
                GraphEndpoint::External(_) | GraphEndpoint::Domain(_) => Ok(Some(Selected {
                    endpoint: endpoint.clone(),
                    declarations: Vec::new(),
                })),
            },
            ProjectionTarget::Resource(selector) => {
                let located = self.index().locate_resource(selector.locator())?;
                self.exact_resource(target, &located, plan, false)
            }
            ProjectionTarget::Symbol(selector) => {
                let located = self.index().search_symbols(&selector.query())?;
                self.exact_symbol(target, &located, plan, false)
            }
        }
    }

    fn exact_resource(
        &self,
        target: &ProjectionTarget,
        located: &Located<crate::resource::Resource>,
        plan: &mut Plan,
        endpoint: bool,
    ) -> Result<Option<Selected>, PlannerError> {
        plan.raw.known(&located.candidates);
        let exact = located.exact().cloned();
        plan.selection(
            target,
            located,
            |resource| GraphEndpoint::Resource(resource.id),
            endpoint,
            exact.is_some(),
        );
        Ok(exact.map(|resource| {
            let endpoint = GraphEndpoint::Resource(resource.id);
            plan.items.push(EvidenceItem::Resource(resource));
            Selected {
                endpoint,
                declarations: Vec::new(),
            }
        }))
    }

    fn exact_symbol(
        &self,
        target: &ProjectionTarget,
        located: &Located<SymbolCandidate>,
        plan: &mut Plan,
        endpoint: bool,
    ) -> Result<Option<Selected>, PlannerError> {
        plan.raw.known(&located.candidates);
        let exact = located.exact().cloned();
        plan.selection(
            target,
            located,
            |candidate| GraphEndpoint::Symbol(candidate.symbol.id),
            endpoint,
            exact.is_some(),
        );
        Ok(exact.map(|candidate| {
            plan.items.push(EvidenceItem::Symbol(candidate.clone()));
            Selected {
                endpoint: GraphEndpoint::Symbol(candidate.symbol.id),
                declarations: vec![candidate],
            }
        }))
    }

    // ---- graph --------------------------------------------------------

    /// The graph part of a change: the I3 plan for a structural change,
    /// direct edges only for anything without one.
    fn change_graph(
        &self,
        anchor: &GraphEndpoint,
        kind: Option<ChangeKind>,
        plan: &mut Plan,
    ) -> Result<(), PlannerError> {
        match kind {
            Some(ChangeKind::Structural(intent)) => self.impact(anchor, intent, plan),
            None => {
                plan.gap(ProjectionGap::DependencyExpansionUndefined);
                self.direct(anchor, &[Direction::Outgoing, Direction::Incoming], plan)
            }
            // No canonical transitive plan: never mapped onto another
            // intent. What depends on the target directly is still known.
            Some(kind @ (ChangeKind::Delete | ChangeKind::DomainContractChange)) => {
                plan.gap(ProjectionGap::UnsupportedImpactProfile(kind));
                self.direct(anchor, &[Direction::Incoming], plan)
            }
        }
    }

    fn direct(
        &self,
        anchor: &GraphEndpoint,
        directions: &[Direction],
        plan: &mut Plan,
    ) -> Result<(), PlannerError> {
        let relations = self.tests.traversal().relations();
        for direction in directions {
            let answer = match direction {
                Direction::Outgoing => relations.outgoing(anchor, &[])?,
                Direction::Incoming => relations.incoming(anchor, &[])?,
            };
            self.count(|stats| stats.relation_queries += 1);
            plan.raw.known(&answer.confirmed);
            plan.raw.known(&answer.gaps);
            let report = answer.coverage.limits();
            let confirmed = answer.confirmed_count();
            plan.relations(answer.confirmed);
            plan.items
                .extend(answer.gaps.into_iter().map(EvidenceItem::RelationGap));
            plan.items.push(EvidenceItem::Coverage(CoverageEvidence {
                subject: CoverageSubject::Relations {
                    anchor: anchor.clone(),
                    direction: *direction,
                    kinds: answer.kinds,
                },
                report,
                confirmed,
            }));
        }
        Ok(())
    }

    /// One I3 traversal, and the related tests derived from that same
    /// result -- never a second walk.
    fn impact(
        &self,
        root: &GraphEndpoint,
        intent: ImpactIntent,
        plan: &mut Plan,
    ) -> Result<(), PlannerError> {
        // The I3 graph-safety budget, unchanged; not a delivery budget.
        let impact = self
            .tests
            .traversal()
            .run(intent, root, &Budget::default())?;
        self.count(|stats| stats.impact_traversals += 1);
        let tests = self.tests.from_impact(&impact)?;
        plan.raw
            .known(impact.edges.iter().map(|edge| &edge.relation));
        plan.raw
            .known(impact.gaps.iter().map(|attributed| &attributed.gap));
        plan.raw.known(&tests.candidates);

        plan.items.push(EvidenceItem::Coverage(CoverageEvidence {
            subject: CoverageSubject::Impact {
                root: root.clone(),
                intent,
            },
            report: impact.limits(),
            confirmed: impact.edges.len(),
        }));
        plan.items.push(EvidenceItem::Coverage(CoverageEvidence {
            subject: CoverageSubject::RelatedTests {
                target: root.clone(),
                intent,
            },
            report: tests.coverage.limits(),
            confirmed: tests.candidates.len(),
        }));
        for edge in &impact.edges {
            let identity = describe(&EvidenceItem::Relation(edge.relation.clone())).3;
            if let Some(identity) = identity {
                let depth = plan.depths.entry(identity).or_insert(edge.depth);
                *depth = (*depth).min(edge.depth);
            }
        }
        plan.relations(impact.edges.into_iter().map(|edge| edge.relation));
        plan.items.extend(
            impact
                .gaps
                .into_iter()
                .map(|attributed| EvidenceItem::RelationGap(attributed.gap)),
        );
        plan.items
            .extend(
                tests
                    .candidates
                    .into_iter()
                    .map(|candidate| EvidenceItem::RelatedTest {
                        target: root.clone(),
                        candidate,
                    }),
            );
        Ok(())
    }

    // ---- knowledge / work ----------------------------------------------

    /// Applicable Policy plus exactly the named subjects, through the
    /// task 2 resolver. Shadowed rows are not projected.
    fn knowledge(
        &self,
        request: &ProjectionRequest,
        context: ApplicabilityContext,
        plan: &mut Plan,
    ) -> Result<(), PlannerError> {
        let refs = &request.knowledge;
        let resolved = self.resolve(&request.directives, refs, context)?;

        let raw = &mut plan.raw;
        raw.known(&resolved.request_directives);
        raw.known(&resolved.protected_constraints);
        raw.known(&resolved.applied_policies);
        raw.known(&resolved.active_decisions);
        raw.known(&resolved.applied_preferences);
        raw.known(&resolved.blueprint_evidence);
        raw.known(&resolved.state_evidence);
        raw.known(&resolved.shadowed);
        raw.known(&resolved.conflicts);

        for gap in not_applied(refs, &resolved) {
            plan.gap(gap);
        }

        let before = plan.items.len();
        decompose_knowledge(resolved, &mut plan.items);
        let selected = (plan.items.len() - before) as u64;
        self.count(|stats| {
            stats.knowledge_resolves += 1;
            stats.knowledge_items += selected;
        });
        Ok(())
    }

    /// The task 2 resolver over this binding's stores, with the request
    /// built exactly one way: applicable Policy by default, other
    /// categories only for the named subjects.
    fn resolve(
        &self,
        directives: &[RequestDirective],
        refs: &ProjectionKnowledgeRefs,
        context: ApplicabilityContext,
    ) -> Result<ResolvedKnowledge, PlannerError> {
        let resolve_request = ResolveRequest {
            directives: directives.to_vec(),
            decision_topics: refs.decision_topics.iter().cloned().collect(),
            preference_keys: refs.preference_keys.iter().cloned().collect(),
            state_keys: refs.state_keys.iter().cloned().collect(),
            blueprint_applications: refs.blueprint_applications.iter().copied().collect(),
            // Working State comes from the WorkRuntime snapshot instead.
            ..ResolveRequest::new(context)
        };
        Ok(resolve(
            &KnowledgeSources {
                global: &self.global,
                project: &self.project,
                workspace: Some(&self.workspace),
            },
            &resolve_request,
        )?)
    }

    /// Applicable rules without a target or WorkItem (#23 knowledge
    /// Rules): the same resolver request and decomposition as a CHANGE /
    /// RESUME plan, in the planner's category order, plus the planner's
    /// not-applied Blueprint gaps. Nothing is cut.
    pub(crate) fn rules(
        &self,
        directives: &[RequestDirective],
        refs: &ProjectionKnowledgeRefs,
        context: ApplicabilityContext,
    ) -> Result<(Vec<EvidenceItem>, Vec<ProjectionGap>), PlannerError> {
        let resolved = self.resolve(directives, refs, context)?;
        let gaps = not_applied(refs, &resolved);
        let mut items = Vec::new();
        decompose_knowledge(resolved, &mut items);
        let items = order(items);
        self.count(|stats| {
            stats.knowledge_resolves += 1;
            stats.knowledge_items += items.len() as u64;
        });
        Ok((items, gaps))
    }

    // ---- task 10 read handles (same verified binding) -------------------

    pub(crate) const fn query_index(&self) -> &QueryIndex {
        self.index()
    }

    pub(crate) const fn relation_index(&self) -> &crate::relations::RelationIndex {
        self.tests.traversal().relations()
    }

    pub(crate) const fn global_store(&self) -> &GlobalKnowledgeStore {
        &self.global
    }

    pub(crate) const fn project_store(&self) -> &ProjectKnowledgeStore {
        &self.project
    }

    pub(crate) const fn workspace_store(&self) -> &WorkspaceKnowledgeStore {
        &self.workspace
    }

    /// The explicit WorkItem's snapshot, decomposed, and its edit overlaps.
    fn work(&self, work_item: WorkItemId, plan: &mut Plan) -> Result<(), PlannerError> {
        let snapshot = self.work.snapshot(work_item, None)?;
        let overlaps = self.work.overlaps(work_item)?;
        self.count(|stats| stats.work_snapshots += 1);
        let raw = &mut plan.raw;
        raw.known([&snapshot.item]);
        raw.known(&snapshot.working_state);
        raw.known(&snapshot.resources);
        raw.known(&snapshot.result);
        raw.known(&snapshot.latest_handoff);
        raw.known(&snapshot.baseline_generation);
        raw.known(&snapshot.result_generation);
        raw.known([&snapshot.staleness]);
        raw.known(&overlaps);

        let items = &mut plan.items;
        items.push(EvidenceItem::WorkItem(snapshot.item));
        items.extend(snapshot.working_state.map(EvidenceItem::WorkingState));
        items.extend(snapshot.result.map(EvidenceItem::WorkResult));
        items.extend(snapshot.latest_handoff.map(EvidenceItem::Handoff));
        for (basis, reference) in [
            (GenerationBasis::Baseline, snapshot.baseline_generation),
            (GenerationBasis::Result, snapshot.result_generation),
        ] {
            items.extend(
                reference.map(|reference| EvidenceItem::GenerationReference {
                    work_item,
                    basis,
                    reference,
                }),
            );
        }
        if snapshot.staleness != Staleness::NotEvaluated {
            items.push(EvidenceItem::WorkStaleness {
                work_item,
                staleness: snapshot.staleness,
            });
        }
        items.extend(
            overlaps
                .into_iter()
                .map(|overlap| EvidenceItem::WorkOverlap { work_item, overlap }),
        );
        Ok(())
    }

    // ---- source ---------------------------------------------------------

    /// Read the required ranges only: exact duplicates dropped, overlaps
    /// in one Resource revision merged, one verified read per Resource.
    fn materialize(&self, plan: &mut Plan) -> Result<(), PlannerError> {
        let merged = merge(&plan.required);
        plan.items.extend(self.read(&merged)?);
        plan.required = merged;
        Ok(())
    }

    /// Read already merged ranges (sorted by Resource, revision, span):
    /// one verified `read_ranges` per Resource revision, results in range
    /// order. A read that cannot be honest becomes `SourceUnavailable`.
    fn read(&self, merged: &[PlannedSourceRange]) -> Result<Vec<EvidenceItem>, PlannerError> {
        let mut items = Vec::with_capacity(merged.len());
        for group in merged
            .chunk_by(|a, b| a.resource == b.resource && a.resource_revision == b.resource_revision)
        {
            let (resource, revision) = (group[0].resource, &group[0].resource_revision);
            let spans: Vec<SourceSpan> = group.iter().map(|range| range.span).collect();
            match self.reader.read_ranges(resource, revision, &spans) {
                Ok(reads) => {
                    let bytes: usize = reads.iter().map(|read| read.source.len()).sum();
                    self.count(|stats| {
                        stats.source_file_reads += 1;
                        stats.source_bytes += bytes as u64;
                    });
                    for (range, read) in group.iter().zip(reads) {
                        items.push(EvidenceItem::CurrentSource(PreparedRange {
                            resource: read.resource_id,
                            path_rel: read.path_rel,
                            resource_revision: read.resource_revision,
                            span: read.effective_span,
                            source: read.source,
                            role: range.role,
                            verification: read.verification,
                        }));
                    }
                }
                Err(error) => {
                    let reason = unavailable_from(error)?;
                    for range in group {
                        items.push(EvidenceItem::SourceUnavailable {
                            resource: range.resource,
                            span: range.span,
                            reason: reason.clone(),
                        });
                    }
                }
            }
        }
        Ok(items)
    }
}

/// Named Blueprint Applications the resolver did not apply, in id order.
fn not_applied(refs: &ProjectionKnowledgeRefs, resolved: &ResolvedKnowledge) -> Vec<ProjectionGap> {
    let applied: BTreeSet<BlueprintApplicationId> = resolved
        .blueprint_evidence
        .iter()
        .map(|entry| entry.item.application.uid)
        .collect();
    refs.blueprint_applications
        .difference(&applied)
        .map(|id| ProjectionGap::BlueprintApplicationNotApplied(*id))
        .collect()
}

/// Resolver output as evidence, in the planner's category order before
/// sorting: protected then applied Policy, directives, Decision,
/// Preference, State, Blueprint, conflicts. Shadowed rows are dropped.
fn decompose_knowledge(resolved: ResolvedKnowledge, items: &mut Vec<EvidenceItem>) {
    items.extend(
        resolved
            .protected_constraints
            .into_iter()
            .map(EvidenceItem::Policy),
    );
    items.extend(
        resolved
            .applied_policies
            .into_iter()
            .map(EvidenceItem::Policy),
    );
    items.extend(
        resolved
            .request_directives
            .into_iter()
            .map(EvidenceItem::Directive),
    );
    items.extend(
        resolved
            .active_decisions
            .into_iter()
            .map(EvidenceItem::Decision),
    );
    items.extend(
        resolved
            .applied_preferences
            .into_iter()
            .map(EvidenceItem::Preference),
    );
    items.extend(
        resolved
            .state_evidence
            .into_iter()
            .map(EvidenceItem::ProjectState),
    );
    items.extend(
        resolved
            .blueprint_evidence
            .into_iter()
            .map(EvidenceItem::Blueprint),
    );
    items.extend(
        resolved
            .conflicts
            .into_iter()
            .map(EvidenceItem::KnowledgeConflict),
    );
}

/// The exact target: its identity and its current local declarations.
struct Selected {
    endpoint: GraphEndpoint,
    declarations: Vec<SymbolCandidate>,
}

// ------------------------------------------------------------------ plan

#[derive(Default)]
struct Plan {
    items: Vec<EvidenceItem>,
    gaps: Vec<ProjectionGap>,
    required: Vec<PlannedSourceRange>,
    /// Shallowest impact depth by relation identity.
    depths: BTreeMap<Vec<u8>, usize>,
    raw: RawTally,
}

/// Raw query output as it was returned: every unit counted; bytes exact
/// only while every unit's payload is already in memory.
#[derive(Default)]
struct RawTally {
    items: usize,
    bytes: usize,
    bytes_unknown: bool,
}

impl RawTally {
    fn known<'a, T: Canonical + 'a>(&mut self, values: impl IntoIterator<Item = &'a T>) {
        for value in values {
            self.items += 1;
            self.bytes += canonical::size(value);
        }
    }

    fn unread(&mut self, count: usize) {
        self.items += count;
        self.bytes_unknown |= count > 0;
    }

    fn amount(&self) -> StageAmount {
        StageAmount {
            items: Measure::Known(self.items),
            bytes: if self.bytes_unknown {
                Measure::Unknown
            } else {
                Measure::Known(self.bytes)
            },
            // No exact counter exists at planning time.
            tokens: Measure::Unknown,
        }
    }
}

impl Plan {
    fn gap(&mut self, gap: ProjectionGap) {
        if !self.gaps.contains(&gap) {
            self.gaps.push(gap);
        }
    }

    fn require_declarations(&mut self, selected: &Selected) {
        // A range candidate's body is not in memory before it is read.
        self.raw.unread(selected.declarations.len());
        for declaration in &selected.declarations {
            let symbol = &declaration.symbol;
            self.required.push(PlannedSourceRange {
                resource: symbol.resource_id,
                resource_revision: symbol.resource_revision.clone(),
                span: symbol.span,
                role: RangeRole::AnchorDeclaration,
                requirement: SourceRequirement::Required,
            });
        }
    }

    fn relations(&mut self, relations: impl IntoIterator<Item = crate::relations::RelationResult>) {
        self.items
            .extend(relations.into_iter().map(EvidenceItem::Relation));
    }

    /// Record how a selector resolved: the selection itself for a
    /// selector (or a failed exact endpoint), its coverage when that is
    /// incomplete or empty, and the gap when there is no exact target.
    fn selection<T>(
        &mut self,
        target: &ProjectionTarget,
        located: &Located<T>,
        endpoint_of: impl Fn(&T) -> GraphEndpoint,
        endpoint: bool,
        exact: bool,
    ) {
        let mut report = CoverageReport::new();
        report.note_if(located.truncated, CoverageLimit::CandidateTruncated);
        for note in &located.incomplete_coverage {
            match note.coverage {
                StructuralCoverage::Complete => {}
                StructuralCoverage::Partial => report.note(CoverageLimit::PartialSupport),
                StructuralCoverage::ContainerOnly
                | StructuralCoverage::Unsupported
                | StructuralCoverage::GeneratedUnmapped => {
                    report.note(CoverageLimit::UnsupportedScope);
                }
            }
        }
        report.note_if(
            !located.currentness.is_current(),
            CoverageLimit::IndexNotCurrent,
        );

        if !endpoint || !exact {
            self.items
                .push(EvidenceItem::TargetSelection(TargetSelection {
                    selector: target.clone(),
                    located: Located {
                        candidates: located.candidates.iter().map(&endpoint_of).collect(),
                        last_valid: located.last_valid.iter().map(&endpoint_of).collect(),
                        exact_selector: located.exact_selector,
                        truncated: located.truncated,
                        currentness: located.currentness,
                        source: located.source,
                        incomplete_coverage: located.incomplete_coverage.clone(),
                    },
                }));
        }
        if !report.is_complete() || located.candidates.is_empty() {
            self.items.push(EvidenceItem::Coverage(CoverageEvidence {
                subject: CoverageSubject::TargetSelection(target.clone()),
                report: report.clone(),
                confirmed: located.candidates.len(),
            }));
        }
        if exact {
            return;
        }
        self.gap(if endpoint {
            ProjectionGap::TargetNotCurrent
        } else if located.candidates.len() > 1 {
            ProjectionGap::TargetAmbiguous
        } else if located.candidates.len() == 1 {
            ProjectionGap::TargetNotExact
        } else if report.is_complete() {
            ProjectionGap::TargetNotFound
        } else {
            ProjectionGap::TargetNotFoundWithIncompleteCoverage
        });
    }

    fn finish(
        mut self,
        workspace: WorkspaceId,
        project: ProjectId,
        intent: ProjectionIntent,
        target: Option<GraphEndpoint>,
    ) -> PreparedProjection {
        let requires_semantics = self.items.iter().any(|item| {
            matches!(item, EvidenceItem::Coverage(coverage)
                if coverage.report.has(CoverageLimit::RequiresSemantics))
        });
        if requires_semantics {
            self.gap(ProjectionGap::RequiresSemantics);
        }

        // Optional candidates: every evidence span of a projected
        // relation, without its body.
        let mut optional = Vec::new();
        for item in &self.items {
            if let EvidenceItem::Relation(relation) = item {
                for location in &relation.evidence {
                    optional.push(PlannedSourceRange {
                        resource: location.resource,
                        resource_revision: location.basis_revision.clone(),
                        span: location.span,
                        role: RangeRole::EvidenceSpan,
                        requirement: SourceRequirement::Optional,
                    });
                }
            }
        }
        self.raw.unread(optional.len());
        optional.sort_by_key(range_key);
        let required_keys: BTreeSet<_> = self.required.iter().map(range_key).collect();
        optional.dedup_by(|a, b| range_key(a) == range_key(b));
        optional.retain(|range| !required_keys.contains(&range_key(range)));
        let mut source_plan = self.required;
        source_plan.extend(optional);

        let evidence = order(self.items);
        let delivery = evidence
            .iter()
            .map(|item| hint(item, &self.depths))
            .collect();
        PreparedProjection {
            workspace,
            project,
            intent,
            target,
            evidence,
            gaps: self.gaps,
            source_plan,
            delivery,
            raw: self.raw.amount(),
        }
    }
}

// ------------------------------------------------------- ranges / order

type RangeKey = ([u8; 16], String, usize, usize);

fn range_key(range: &PlannedSourceRange) -> RangeKey {
    (
        range.resource.to_bytes(),
        range.resource_revision.clone(),
        range.span.start_byte,
        range.span.end_byte,
    )
}

const fn role_rank(role: RangeRole) -> u8 {
    match role {
        RangeRole::AnchorDeclaration => 0,
        RangeRole::ContainingDeclaration => 1,
        RangeRole::EvidenceSpan => 2,
    }
}

/// Exact duplicates collapse; ranges of one Resource revision that
/// overlap merge into their union (never across a gap), keeping the
/// strongest role. Deterministic: sorted by (Resource, revision, span).
fn merge(ranges: &[PlannedSourceRange]) -> Vec<PlannedSourceRange> {
    let mut sorted = ranges.to_vec();
    sorted.sort_by_key(range_key);
    let mut merged: Vec<PlannedSourceRange> = Vec::new();
    for range in sorted {
        if let Some(last) = merged.last_mut()
            && last.resource == range.resource
            && last.resource_revision == range.resource_revision
            && (range_key(&range) == range_key(last) || range.span.start_byte < last.span.end_byte)
        {
            if range.span.end_byte > last.span.end_byte {
                last.span.end_byte = range.span.end_byte;
                last.span.end = range.span.end;
            }
            if role_rank(range.role) < role_rank(last.role) {
                last.role = range.role;
            }
            continue;
        }
        merged.push(range);
    }
    merged
}

fn be(value: usize) -> [u8; 8] {
    (value as u64).to_be_bytes()
}

fn endpoint_bytes(endpoint: &GraphEndpoint) -> Vec<u8> {
    let (kind, mut bytes) = graph::endpoint_sort_key(endpoint);
    bytes.insert(0, kind);
    bytes
}

fn span_bytes(span: SourceSpan) -> Vec<u8> {
    [be(span.start_byte), be(span.end_byte)].concat()
}

/// (category, variant, sort key, identity). An empty sort key keeps the
/// producing surface's own deterministic order; `None` identity is never
/// deduplicated (one per query by construction).
fn describe(item: &EvidenceItem) -> (u8, u8, Vec<u8>, Option<Vec<u8>>) {
    let id = |bytes: [u8; 16]| Some(bytes.to_vec());
    match item {
        EvidenceItem::Resource(resource) => (
            1,
            0,
            resource.path_key.as_bytes().to_vec(),
            id(resource.id.to_bytes()),
        ),
        EvidenceItem::Symbol(candidate) => (
            1,
            1,
            [
                candidate.path_rel.as_bytes(),
                &[0],
                &span_bytes(candidate.symbol.span),
            ]
            .concat(),
            id(candidate.symbol.id.to_bytes()),
        ),
        EvidenceItem::TargetSelection(_) => (1, 2, Vec::new(), None),
        EvidenceItem::CurrentSource(range) => {
            let key = [
                &range.resource.to_bytes()[..],
                range.resource_revision.as_bytes(),
                &[0],
                &span_bytes(range.span),
            ]
            .concat();
            (
                2,
                0,
                [range.path_rel.as_bytes(), &[0], &span_bytes(range.span)].concat(),
                Some(key),
            )
        }
        EvidenceItem::Policy(entry) => (3, 0, Vec::new(), id(entry.item.uid.to_bytes())),
        EvidenceItem::Directive(entry) => {
            (3, 1, Vec::new(), Some(entry.item.id.clone().into_bytes()))
        }
        EvidenceItem::Decision(entry) => (4, 0, Vec::new(), id(entry.item.uid.to_bytes())),
        EvidenceItem::Preference(entry) => (4, 1, Vec::new(), id(entry.item.uid.to_bytes())),
        EvidenceItem::ProjectState(entry) => (4, 2, Vec::new(), id(entry.item.uid.to_bytes())),
        EvidenceItem::Blueprint(entry) => {
            (4, 3, Vec::new(), id(entry.item.application.uid.to_bytes()))
        }
        EvidenceItem::Relation(relation) => {
            let key = [
                relation.kind.as_str().as_bytes(),
                &[0],
                &endpoint_bytes(&relation.source),
                &[0],
                &endpoint_bytes(&relation.target),
            ]
            .concat();
            (5, 0, key.clone(), Some(key))
        }
        EvidenceItem::RelationGap(gap) => {
            let key = [
                &gap.location.resource.to_bytes()[..],
                &span_bytes(gap.location.span),
                gap.lookup_name.as_bytes(),
            ]
            .concat();
            (5, 1, key.clone(), Some(key))
        }
        EvidenceItem::RelatedTest { target, candidate } => {
            let key = [
                &endpoint_bytes(target)[..],
                &[0],
                &candidate.resource.to_bytes(),
            ]
            .concat();
            (6, 0, candidate.path_rel.as_bytes().to_vec(), Some(key))
        }
        EvidenceItem::WorkItem(item) => (7, 0, Vec::new(), id(item.uid.to_bytes())),
        EvidenceItem::WorkingState(state) => (7, 1, Vec::new(), id(state.work_item.to_bytes())),
        EvidenceItem::WorkResult(result) => (7, 2, Vec::new(), id(result.work_item.to_bytes())),
        EvidenceItem::Handoff(handoff) => (7, 3, Vec::new(), id(handoff.work_item.to_bytes())),
        EvidenceItem::GenerationReference {
            work_item, basis, ..
        } => (
            7,
            4,
            Vec::new(),
            Some([&work_item.to_bytes()[..], &[*basis as u8]].concat()),
        ),
        EvidenceItem::WorkStaleness { work_item, .. } => {
            (7, 5, Vec::new(), id(work_item.to_bytes()))
        }
        EvidenceItem::WorkOverlap { work_item, overlap } => (
            7,
            6,
            Vec::new(),
            Some(
                [
                    &work_item.to_bytes()[..],
                    &overlap.other.to_bytes(),
                    &overlap.resource.to_bytes(),
                ]
                .concat(),
            ),
        ),
        EvidenceItem::KnowledgeConflict(conflict) => {
            let mut key = vec![conflict.kind as u8];
            key.extend(conflict.subject.as_deref().unwrap_or_default().as_bytes());
            for involved in &conflict.involved {
                key.push(0);
                key.extend(involved.id.as_bytes());
            }
            (8, 0, Vec::new(), Some(key))
        }
        EvidenceItem::SourceUnavailable { resource, span, .. } => {
            let key = [&resource.to_bytes()[..], &span_bytes(*span)].concat();
            (8, 1, key.clone(), Some(key))
        }
        EvidenceItem::Coverage(_) => (8, 2, Vec::new(), None),
        EvidenceItem::IndexCurrentness { .. } => (8, 3, Vec::new(), None),
    }
}

fn hint(item: &EvidenceItem, depths: &BTreeMap<Vec<u8>, usize>) -> DeliveryHint {
    let (category, _, _, identity) = describe(item);
    let impact_depth = identity.and_then(|identity| depths.get(&identity).copied());
    let relevance = match (category, item) {
        (5, EvidenceItem::Relation(_)) if impact_depth.unwrap_or(0) > 0 => {
            Relevance::TransitiveImpact
        }
        (5, EvidenceItem::Relation(_)) => Relevance::DirectRelation,
        (5, _) => Relevance::CoverageSupport,
        (1 | 2, _) | (_, EvidenceItem::IndexCurrentness { .. }) => Relevance::Target,
        (3, _) => Relevance::ApplicableRule,
        (4, _) => Relevance::RequestedKnowledge,
        (6, _) => Relevance::RelatedTest,
        (7, _) => Relevance::WorkingState,
        _ => Relevance::Corrective,
    };
    DeliveryHint {
        relevance,
        impact_depth,
    }
}

/// (category, variant, sort key).
type OrderKey = (u8, u8, Vec<u8>);

/// Deduplicate by canonical identity (first occurrence wins), then a
/// stable sort into category order.
fn order(items: Vec<EvidenceItem>) -> Vec<EvidenceItem> {
    let mut seen = BTreeSet::new();
    let mut keyed: Vec<(OrderKey, EvidenceItem)> = Vec::new();
    for item in items {
        let (category, variant, sort, identity) = describe(&item);
        if let Some(identity) = identity
            && !seen.insert((category, variant, identity))
        {
            continue;
        }
        keyed.push(((category, variant, sort), item));
    }
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    keyed.into_iter().map(|(_, item)| item).collect()
}

mod delivery;
mod economy;

pub use delivery::{
    ContinuationMismatch, ContinuationUnavailable, DeliveryBudget, DeliveryContinuation,
    DeliveryDimension, DeliveryError, DeliveryKey, DeliveryPage, DeliveryUnit, ExactTokenCounter,
    TokenUsage,
};
pub use economy::{
    Acknowledged, ContextRetention, DeliveryLedger, DeliveryReceipt, DeliveryScope,
    FallbackObservation, LedgerLimits, LedgerLimitsError, Measure, PendingDelivery,
    ProjectionEconomy, ReuseIdentity, ReuseObservation, ReuseReference, StageAmount,
};

#[cfg(test)]
mod tests;
