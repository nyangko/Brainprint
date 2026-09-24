//! High-level Core query surface (#20 task 10, #23).
//!
//! One Workspace-bound handle and seven typed operations -- `find`,
//! `inspect`, `relations`, `impact`, `context`, `knowledge`, `structure`
//! -- each a fixed mapping onto an existing lower-level primitive:
//! target selection, projection, delivery, continuation and reuse are the
//! task 6/7/8 planner's; rules are the task 2 resolver's; Working State is
//! the task 3 store's; structure is task 9's summary. Nothing here ranks,
//! picks among candidates, pages on its own, caches, or initializes.
//!
//! Transport-neutral: adapters (task 11/12) encode these types without
//! reinterpreting them, and own the ledger lifetime and acknowledgement.

use std::{
    cell::Cell,
    error::Error,
    fmt,
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use brainprint_core::{DecisionId, PolicyId, WorkItemId, WorkspaceId};

use crate::{
    boundary::{
        StructuralSummary, StructuralSummaryIndex, StructuralSummaryRequest, SummaryError,
        SummaryRequestError, SummaryStats,
    },
    config::{ConfigError, WorkspaceConfig, load_workspace_config},
    graph::{GraphEndpoint, RelationKind},
    knowledge::{
        DecisionLineage, KnowledgeError, KnowledgeScope, PolicyLineage, RequestDirective,
        WorkError, WorkHandoff, WorkItem, WorkItemStatus,
    },
    paths::WorkspacePaths,
    projection::{
        ChangeKind, EvidenceItem, PlannerError, PlannerStats, PreparedProjection,
        ProjectionCorrelation, ProjectionGap, ProjectionIntent, ProjectionKnowledgeRefs,
        ProjectionPlanner, ProjectionRequest, ProjectionRequestError, ProjectionTarget,
        planner::{
            ContextRetention, DeliveryBudget, DeliveryContinuation, DeliveryError, DeliveryLedger,
            ExactTokenCounter, PendingDelivery,
        },
        validate_applicability,
    },
    query::{Currentness, DEFAULT_CANDIDATE_LIMIT, FileListing, FileQuery, QueryError},
    registry::{GlobalRegistry, RegistryError},
    relations::{RelationAnswer, RelationError},
    resource::{ResourceKind, ResourceLanguage, ResourceRole},
    search::{
        FallbackReason, SearchBudget, SearchError, TextPattern, TextSearch, TextSearchResult,
        TextSearcher,
    },
};

/// Implementation limit on one `knowledge` WorkItem listing (per status).
pub const MAX_WORK_ITEM_LIST: usize = 200;
/// Implementation limit on one `knowledge` handoff history.
pub const MAX_HANDOFF_HISTORY: usize = 50;

// ------------------------------------------------------ shared request

/// Every request's correctness identity. The Project comes from the
/// registry binding; no revision token is asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryContext {
    pub workspace: WorkspaceId,
    /// Opaque; drives the task 8 delivery scope only.
    pub correlation: Option<ProjectionCorrelation>,
}

/// Task 7/8 delivery inputs of a planner-backed operation, all supplied
/// by the caller: there is no default budget, page size or retention.
#[derive(Clone)]
pub struct DeliveryOptions<'a> {
    pub budget: DeliveryBudget,
    /// `None` for the first page; else the one the previous page returned.
    pub continuation: Option<DeliveryContinuation>,
    pub retention: ContextRetention,
    /// Required iff the budget has a token cap.
    pub tokens: Option<&'a dyn ExactTokenCounter>,
}

// ---------------------------------------------------------- outcomes

/// How the target resolved, read from the planner's own selection
/// result. Never a candidate the planner did not resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetResolution {
    Resolved(GraphEndpoint),
    /// Several candidates; see the `TargetSelection` evidence.
    MultipleCandidates,
    /// A search selector matched once; not promoted to an identity.
    SingleNonExactCandidate,
    /// No candidate under complete selection coverage.
    NotFound,
    NotFoundIncompleteCoverage,
    /// An exact identity that is not current in the index.
    NotCurrent,
    /// A resume without a target.
    NoTarget,
}

impl TargetResolution {
    fn of(projection: &PreparedProjection) -> Self {
        if let Some(target) = &projection.target {
            return Self::Resolved(target.clone());
        }
        projection
            .gaps
            .iter()
            .find_map(|gap| match gap {
                ProjectionGap::TargetAmbiguous => Some(Self::MultipleCandidates),
                ProjectionGap::TargetNotExact => Some(Self::SingleNonExactCandidate),
                ProjectionGap::TargetNotFound => Some(Self::NotFound),
                ProjectionGap::TargetNotFoundWithIncompleteCoverage => {
                    Some(Self::NotFoundIncompleteCoverage)
                }
                ProjectionGap::TargetNotCurrent => Some(Self::NotCurrent),
                _ => None,
            })
            .unwrap_or(Self::NoTarget)
    }
}

/// The projection's `IndexCurrentness` item; the planner emits one
/// whenever the index is not current.
fn currentness(evidence: &[EvidenceItem]) -> Currentness {
    evidence
        .iter()
        .find_map(|item| match item {
            EvidenceItem::IndexCurrentness { currentness, .. } => Some(*currentness),
            _ => None,
        })
        .unwrap_or(Currentness::Current)
}

/// A planner-backed answer: outcome and currentness from the whole
/// projection (so they hold on every page), plus the task 8 pending page.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedAnswer {
    pub target: TargetResolution,
    pub currentness: Currentness,
    /// Not delivered until the adapter acknowledges its receipt.
    pub delivery: PendingDelivery,
}

// -------------------------------------------------------------- find

/// Owned form of [`TextPattern`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnedTextPattern {
    Literal(String),
    Regex(String),
}

pub struct FindRequest<'a> {
    pub context: QueryContext,
    pub query: FindQuery<'a>,
}

// One request/answer per call, moved once: boxing buys nothing.
#[allow(clippy::large_enum_variant)]
pub enum FindQuery<'a> {
    /// Candidates for a selector. Reads no source.
    Target {
        target: ProjectionTarget,
        delivery: DeliveryOptions<'a>,
    },
    /// Resource inventory from the index. Reads no source.
    Files {
        directory: Option<String>,
        recursive: bool,
        path_prefix: Option<String>,
        role: Option<ResourceRole>,
        language: Option<ResourceLanguage>,
        kind: Option<ResourceKind>,
        /// At most [`DEFAULT_CANDIDATE_LIMIT`].
        limit: NonZeroUsize,
    },
    /// Explicit text search over current files; never run on its own
    /// after a structured miss.
    Text {
        pattern: OwnedTextPattern,
        case_insensitive: bool,
        path_prefix: Option<String>,
        budget: SearchBudget,
        max_file_bytes: u64,
        with_preview: bool,
    },
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum FindResult {
    Target(ProjectedAnswer),
    Files(FileListing),
    Text(TextSearchResult),
}

// ---------------------------------------------------- inspect / impact

pub struct InspectRequest<'a> {
    pub context: QueryContext,
    pub target: ProjectionTarget,
    pub delivery: DeliveryOptions<'a>,
}

pub struct ImpactRequest<'a> {
    pub context: QueryContext,
    pub target: ProjectionTarget,
    pub change: ChangeKind,
    pub delivery: DeliveryOptions<'a>,
}

// --------------------------------------------------------- relations

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationDirection {
    Outgoing,
    Incoming,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationsRequest {
    pub context: QueryContext,
    pub target: ProjectionTarget,
    pub direction: RelationDirection,
    /// Empty means every kind (the `RelationIndex` rule).
    pub kinds: Vec<RelationKind>,
}

/// One anchor's whole direct answer, unpaged (#23 decision 1).
#[derive(Debug, Clone, PartialEq)]
pub struct RelationsResult {
    pub target: TargetResolution,
    /// The LOCATE plan's selection, coverage and currentness items.
    pub selection: Vec<EvidenceItem>,
    pub currentness: Currentness,
    /// Outgoing then incoming; empty unless `Resolved`.
    pub answers: Vec<RelationAnswer>,
}

// ----------------------------------------------------------- context

pub struct ContextRequest<'a> {
    pub context: QueryContext,
    pub purpose: ContextPurpose,
    pub scope_layers: Vec<Vec<KnowledgeScope>>,
    pub directives: Vec<RequestDirective>,
    pub knowledge: ProjectionKnowledgeRefs,
    pub delivery: DeliveryOptions<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextPurpose {
    Change {
        target: ProjectionTarget,
        change: Option<ChangeKind>,
        work_item: Option<WorkItemId>,
    },
    Resume {
        work_item: WorkItemId,
        target: Option<ProjectionTarget>,
    },
}

// --------------------------------------------------------- knowledge

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeRequest {
    pub context: QueryContext,
    pub query: KnowledgeQuery,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KnowledgeQuery {
    /// Current applicable rules without a target or WorkItem.
    Rules {
        scope_layers: Vec<Vec<KnowledgeScope>>,
        directives: Vec<RequestDirective>,
        knowledge: ProjectionKnowledgeRefs,
    },
    /// Current WorkItems by explicit status; lists, never selects.
    /// Canonicalized inside Core: deduplicated and ordered by the fixed
    /// vocabulary order (`STATUS_ORDER`), independent of the caller's
    /// order or duplicates -- two requests naming the same set are the
    /// same request.
    WorkItems {
        statuses: Vec<WorkItemStatus>,
        /// Per status, at most [`MAX_WORK_ITEM_LIST`].
        limit: NonZeroUsize,
    },
    /// One-hop history of an exact id.
    Lineage(LineageTarget),
    /// Handoff history, newest first.
    Handoffs {
        work_item: WorkItemId,
        /// At most [`MAX_HANDOFF_HISTORY`].
        limit: NonZeroUsize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineageTarget {
    ProjectPolicy(PolicyId),
    UserPolicy(PolicyId),
    Decision(DecisionId),
}

#[derive(Debug, Clone, PartialEq)]
pub enum KnowledgeResult {
    /// Resolver output in the planner's order, plus the planner's
    /// not-applied Blueprint gaps.
    Rules {
        evidence: Vec<EvidenceItem>,
        gaps: Vec<ProjectionGap>,
    },
    WorkItems {
        items: Vec<WorkItem>,
        truncated: bool,
    },
    PolicyLineage(PolicyLineage),
    DecisionLineage(DecisionLineage),
    Handoffs {
        work_item: WorkItemId,
        handoffs: Vec<WorkHandoff>,
        truncated: bool,
    },
}

// ------------------------------------------------------------- error

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotInitialized {
    GlobalDbMissing,
    WorkspaceNotRegistered,
    WorkspaceDbMissing,
    IndexDbMissing,
    WorkspaceUnbound { db: &'static str },
}

#[derive(Debug)]
pub enum InvalidRequest {
    Projection(ProjectionRequestError),
    Summary(SummaryRequestError),
    ListLimitTooLarge {
        limit: usize,
        max: usize,
    },
    /// A zero search budget axis or file-size cap.
    SearchBudgetInvalid,
}

#[derive(Debug)]
pub enum CoreError {
    /// Never answered by initializing or syncing anything.
    NotInitialized(NotInitialized),
    WorkspaceRootMissing {
        workspace: WorkspaceId,
    },
    WorkspaceLocatorAmbiguous {
        workspaces: Vec<WorkspaceId>,
    },
    WorkspaceMismatch {
        bound: WorkspaceId,
        requested: WorkspaceId,
    },
    WorkspaceBindingMismatch {
        db: &'static str,
        expected: WorkspaceId,
        found: WorkspaceId,
    },
    /// The explicit WorkItem is not in the bound workspace.db.
    WorkItemNotFound {
        work_item: WorkItemId,
    },
    InvalidRequest(InvalidRequest),
    Delivery(DeliveryError),
    Planner(PlannerError),
    Relation(RelationError),
    Knowledge(KnowledgeError),
    Work(WorkError),
    Query(QueryError),
    Search(SearchError),
    Summary(SummaryError),
    Registry(RegistryError),
    Config(ConfigError),
}

impl fmt::Display for CoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInitialized(reason) => write!(formatter, "not initialized: {reason:?}"),
            Self::WorkspaceRootMissing { workspace } => {
                write!(
                    formatter,
                    "the root of Workspace {workspace} does not exist"
                )
            }
            Self::WorkspaceLocatorAmbiguous { workspaces } => write!(
                formatter,
                "{} Workspaces are registered at this locator",
                workspaces.len()
            ),
            Self::WorkspaceMismatch { bound, requested } => write!(
                formatter,
                "bound to Workspace {bound}, request names {requested}"
            ),
            Self::WorkspaceBindingMismatch {
                db,
                expected,
                found,
            } => write!(
                formatter,
                "{db} belongs to Workspace {found}, not {expected}"
            ),
            Self::WorkItemNotFound { work_item } => {
                write!(formatter, "WorkItem {work_item} is not in this Workspace")
            }
            Self::InvalidRequest(reason) => write!(formatter, "invalid request: {reason:?}"),
            Self::Delivery(source) => write!(formatter, "{source}"),
            Self::Planner(source) => write!(formatter, "{source}"),
            Self::Relation(source) => write!(formatter, "{source}"),
            Self::Knowledge(source) => write!(formatter, "{source}"),
            Self::Work(source) => write!(formatter, "{source}"),
            Self::Query(source) => write!(formatter, "{source}"),
            Self::Search(source) => write!(formatter, "{source}"),
            Self::Summary(source) => write!(formatter, "{source}"),
            Self::Registry(source) => write!(formatter, "{source}"),
            Self::Config(source) => write!(formatter, "{source}"),
        }
    }
}

impl Error for CoreError {}

impl CoreError {
    /// The #23 mapping. `work_item` is the request's explicit WorkItem,
    /// named by a not-found error.
    fn planner(error: PlannerError, workspace: WorkspaceId, work_item: Option<WorkItemId>) -> Self {
        match error {
            PlannerError::MissingGlobalDb => Self::NotInitialized(NotInitialized::GlobalDbMissing),
            PlannerError::UnknownWorkspace(_) => {
                Self::NotInitialized(NotInitialized::WorkspaceNotRegistered)
            }
            PlannerError::MissingWorkspaceRoot => Self::WorkspaceRootMissing { workspace },
            PlannerError::WorkspaceMismatch { bound, requested } => {
                Self::WorkspaceMismatch { bound, requested }
            }
            PlannerError::Request(error) => Self::InvalidRequest(InvalidRequest::Projection(error)),
            PlannerError::Work(error) => Self::work(error, work_item),
            other => Self::Planner(other),
        }
    }

    fn work(error: WorkError, work_item: Option<WorkItemId>) -> Self {
        match error {
            WorkError::MissingDatabase { db: "workspace.db" } => {
                Self::NotInitialized(NotInitialized::WorkspaceDbMissing)
            }
            WorkError::MissingDatabase { db: "index.db" } => {
                Self::NotInitialized(NotInitialized::IndexDbMissing)
            }
            WorkError::UnboundWorkspace { db } => {
                Self::NotInitialized(NotInitialized::WorkspaceUnbound { db })
            }
            WorkError::WorkspaceMismatch {
                db,
                expected,
                found,
            } => Self::WorkspaceBindingMismatch {
                db,
                expected,
                found,
            },
            WorkError::Knowledge(error) => Self::knowledge(error, work_item),
            other => Self::Work(other),
        }
    }

    fn knowledge(error: KnowledgeError, work_item: Option<WorkItemId>) -> Self {
        match (error, work_item) {
            (
                KnowledgeError::NotFound {
                    what: "work_item", ..
                },
                Some(work_item),
            ) => Self::WorkItemNotFound { work_item },
            (error, _) => Self::Knowledge(error),
        }
    }
}

// ------------------------------------------------------------- stats

/// Lower-level calls the surface made, one counter per primitive. Test
/// and benchmark observation only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SurfaceStats {
    pub plans: u64,
    pub deliveries: u64,
    pub relation_queries: u64,
    pub file_listings: u64,
    pub text_searches: u64,
    pub rule_resolves: u64,
    pub work_item_lists: u64,
    pub lineage_reads: u64,
    pub handoff_lists: u64,
    pub summaries: u64,
}

// ----------------------------------------------------------- surface

/// The Core query surface bound to one Workspace.
pub struct CoreQuerySurface {
    workspace: WorkspaceId,
    root: PathBuf,
    config: WorkspaceConfig,
    planner: ProjectionPlanner,
    summary: StructuralSummaryIndex,
    stats: Cell<SurfaceStats>,
}

impl CoreQuerySurface {
    /// Bind through the planner's registry binding (which checks every
    /// database exists and is bound to these identities), then open the
    /// read handles the non-planner operations need on the same paths.
    /// Nothing is created, initialized or repaired.
    pub fn open(global_db: &Path, workspace: WorkspaceId) -> Result<Self, CoreError> {
        let planner = ProjectionPlanner::open(global_db, workspace)
            .map_err(|error| CoreError::planner(error, workspace, None))?;
        let entry = GlobalRegistry::open(global_db)
            .and_then(|registry| registry.get_workspace(workspace))
            .map_err(CoreError::Registry)?
            .ok_or(CoreError::NotInitialized(
                NotInitialized::WorkspaceNotRegistered,
            ))?;
        let paths = WorkspacePaths::from_root(&entry.locator);
        Ok(Self {
            workspace,
            config: load_workspace_config(&paths).map_err(CoreError::Config)?,
            summary: StructuralSummaryIndex::open(&paths.index_db).map_err(CoreError::Summary)?,
            root: entry.locator,
            planner,
            stats: Cell::new(SurfaceStats::default()),
        })
    }

    /// The one Workspace registered at `locator`. Never picks among
    /// several and never registers one.
    pub fn resolve_workspace(global_db: &Path, locator: &Path) -> Result<WorkspaceId, CoreError> {
        if !global_db.is_file() {
            return Err(CoreError::NotInitialized(NotInitialized::GlobalDbMissing));
        }
        let candidates = GlobalRegistry::open(global_db)
            .and_then(|registry| registry.find_by_locator(locator))
            .map_err(CoreError::Registry)?;
        match candidates.workspaces.as_slice() {
            [] => Err(CoreError::NotInitialized(
                NotInitialized::WorkspaceNotRegistered,
            )),
            [one] => Ok(one.workspace_id),
            several => Err(CoreError::WorkspaceLocatorAmbiguous {
                workspaces: several.iter().map(|entry| entry.workspace_id).collect(),
            }),
        }
    }

    #[must_use]
    pub const fn workspace(&self) -> WorkspaceId {
        self.workspace
    }

    #[must_use]
    pub fn stats(&self) -> SurfaceStats {
        self.stats.get()
    }

    #[must_use]
    pub fn planner_stats(&self) -> PlannerStats {
        self.planner.stats()
    }

    #[must_use]
    pub fn summary_stats(&self) -> SummaryStats {
        self.summary.stats()
    }

    fn count(&self, update: impl FnOnce(&mut SurfaceStats)) {
        let mut stats = self.stats.get();
        update(&mut stats);
        self.stats.set(stats);
    }

    fn check(&self, requested: WorkspaceId) -> Result<(), CoreError> {
        if requested == self.workspace {
            Ok(())
        } else {
            Err(CoreError::WorkspaceMismatch {
                bound: self.workspace,
                requested,
            })
        }
    }

    fn request(&self, context: &QueryContext, intent: ProjectionIntent) -> ProjectionRequest {
        let mut request = ProjectionRequest::new(context.workspace, intent);
        request.correlation.clone_from(&context.correlation);
        request
    }

    fn plan(&self, request: &ProjectionRequest) -> Result<PreparedProjection, CoreError> {
        let projection = self
            .planner
            .plan(request)
            .map_err(|error| CoreError::planner(error, self.workspace, request.work_item))?;
        self.count(|stats| stats.plans += 1);
        Ok(projection)
    }

    /// Plan, then one task 7/8 page. Every call re-plans: nothing is held
    /// between calls, so reuse is always judged on freshly verified truth.
    fn project(
        &self,
        request: &ProjectionRequest,
        options: &DeliveryOptions<'_>,
        ledger: &mut DeliveryLedger,
    ) -> Result<ProjectedAnswer, CoreError> {
        let projection = self.plan(request)?;
        let delivery = self
            .planner
            .deliver_pending(
                request,
                &projection,
                &options.budget,
                options.continuation.as_ref(),
                options.tokens,
                ledger,
                options.retention,
            )
            .map_err(CoreError::Delivery)?;
        self.count(|stats| stats.deliveries += 1);
        Ok(ProjectedAnswer {
            target: TargetResolution::of(&projection),
            currentness: currentness(&projection.evidence),
            delivery,
        })
    }

    // ---- operations ------------------------------------------------------

    pub fn find(
        &self,
        request: FindRequest<'_>,
        ledger: &mut DeliveryLedger,
    ) -> Result<FindResult, CoreError> {
        self.check(request.context.workspace)?;
        match request.query {
            FindQuery::Target { target, delivery } => {
                let mut projection = self.request(&request.context, ProjectionIntent::Locate);
                projection.target = Some(target);
                self.project(&projection, &delivery, ledger)
                    .map(FindResult::Target)
            }
            FindQuery::Files {
                directory,
                recursive,
                path_prefix,
                role,
                language,
                kind,
                limit,
            } => {
                limit_at_most(limit, DEFAULT_CANDIDATE_LIMIT)?;
                let listing = self
                    .planner
                    .query_index()
                    .list_files(&FileQuery {
                        directory: directory.as_deref(),
                        recursive,
                        path_prefix: path_prefix.as_deref(),
                        role,
                        language,
                        kind,
                        limit: Some(limit.get()),
                    })
                    .map_err(CoreError::Query)?;
                self.count(|stats| stats.file_listings += 1);
                Ok(FindResult::Files(listing))
            }
            FindQuery::Text {
                pattern,
                case_insensitive,
                path_prefix,
                budget,
                max_file_bytes,
                with_preview,
            } => {
                let zero = budget.max_results == 0
                    || budget.max_files == 0
                    || budget.max_bytes == 0
                    || budget.deadline.is_some_and(|deadline| deadline.is_zero())
                    || max_file_bytes == 0;
                if zero {
                    return Err(CoreError::InvalidRequest(
                        InvalidRequest::SearchBudgetInvalid,
                    ));
                }
                let search = TextSearch {
                    pattern: match &pattern {
                        OwnedTextPattern::Literal(text) => TextPattern::Literal(text),
                        OwnedTextPattern::Regex(text) => TextPattern::Regex(text),
                    },
                    case_insensitive,
                    path_prefix: path_prefix.as_deref(),
                    reason: FallbackReason::ExplicitTextSearch,
                    after_structured: None,
                    budget,
                    max_file_bytes,
                    with_preview,
                };
                let result =
                    TextSearcher::new(&self.root, &self.config, self.planner.query_index())
                        .search(&search)
                        .map_err(CoreError::Search)?;
                self.count(|stats| stats.text_searches += 1);
                Ok(FindResult::Text(result))
            }
        }
    }

    /// UNDERSTAND: the target with its exact current declaration source
    /// (or `SourceUnavailable`) and both directions of direct relations.
    pub fn inspect(
        &self,
        request: InspectRequest<'_>,
        ledger: &mut DeliveryLedger,
    ) -> Result<ProjectedAnswer, CoreError> {
        self.check(request.context.workspace)?;
        let mut projection = self.request(&request.context, ProjectionIntent::Understand);
        projection.target = Some(request.target);
        self.project(&projection, &request.delivery, ledger)
    }

    /// Direct confirmed relations of one resolved anchor, index-only and
    /// unpaged: the whole `RelationIndex` answer per requested direction.
    pub fn relations(&self, request: RelationsRequest) -> Result<RelationsResult, CoreError> {
        self.check(request.context.workspace)?;
        let mut locate = self.request(&request.context, ProjectionIntent::Locate);
        locate.target = Some(request.target);
        let projection = self.plan(&locate)?;
        let target = TargetResolution::of(&projection);
        let mut answers = Vec::new();
        if let TargetResolution::Resolved(anchor) = &target {
            let index = self.planner.relation_index();
            let (outgoing, incoming) = match request.direction {
                RelationDirection::Outgoing => (true, false),
                RelationDirection::Incoming => (false, true),
                RelationDirection::Both => (true, true),
            };
            if outgoing {
                answers.push(
                    index
                        .outgoing(anchor, &request.kinds)
                        .map_err(CoreError::Relation)?,
                );
                self.count(|stats| stats.relation_queries += 1);
            }
            if incoming {
                answers.push(
                    index
                        .incoming(anchor, &request.kinds)
                        .map_err(CoreError::Relation)?,
                );
                self.count(|stats| stats.relation_queries += 1);
            }
        }
        Ok(RelationsResult {
            target,
            currentness: currentness(&projection.evidence),
            selection: projection.evidence,
            answers,
        })
    }

    /// IMPACT: the I3 traversal and related tests for a declared change
    /// form, as change-candidate evidence with its coverage.
    pub fn impact(
        &self,
        request: ImpactRequest<'_>,
        ledger: &mut DeliveryLedger,
    ) -> Result<ProjectedAnswer, CoreError> {
        self.check(request.context.workspace)?;
        let mut projection =
            self.request(&request.context, ProjectionIntent::Impact(request.change));
        projection.target = Some(request.target);
        self.project(&projection, &request.delivery, ledger)
    }

    /// CHANGE or RESUME_HANDOFF: one projection request built only from
    /// these fields, so a continuation matches on the next page.
    pub fn context(
        &self,
        request: ContextRequest<'_>,
        ledger: &mut DeliveryLedger,
    ) -> Result<ProjectedAnswer, CoreError> {
        self.check(request.context.workspace)?;
        let (intent, target, work_item) = match request.purpose {
            ContextPurpose::Change {
                target,
                change,
                work_item,
            } => (ProjectionIntent::Change(change), Some(target), work_item),
            ContextPurpose::Resume { work_item, target } => {
                (ProjectionIntent::ResumeHandoff, target, Some(work_item))
            }
        };
        let mut projection = self.request(&request.context, intent);
        projection.target = target;
        projection.work_item = work_item;
        projection.scope_layers = request.scope_layers;
        projection.directives = request.directives;
        projection.knowledge = request.knowledge;
        self.project(&projection, &request.delivery, ledger)
    }

    pub fn knowledge(&self, request: KnowledgeRequest) -> Result<KnowledgeResult, CoreError> {
        let context = request.context;
        self.check(context.workspace)?;
        match request.query {
            KnowledgeQuery::Rules {
                scope_layers,
                directives,
                knowledge,
            } => {
                let applicability = validate_applicability(
                    context.workspace,
                    &scope_layers,
                    &directives,
                    &knowledge,
                    context.correlation.as_ref(),
                )
                .map_err(|error| CoreError::InvalidRequest(InvalidRequest::Projection(error)))?;
                let (evidence, gaps) =
                    self.planner
                        .rules(&directives, &knowledge, applicability)
                        .map_err(|error| CoreError::planner(error, self.workspace, None))?;
                self.count(|stats| stats.rule_resolves += 1);
                Ok(KnowledgeResult::Rules { evidence, gaps })
            }
            KnowledgeQuery::WorkItems { statuses, limit } => {
                let limit = limit_at_most(limit, MAX_WORK_ITEM_LIST)?;
                let (mut items, mut truncated) = (Vec::new(), false);
                for status in canonical_statuses(&statuses) {
                    let mut page = self
                        .planner
                        .workspace_store()
                        .list_work_items(status, limit + 1)
                        .map_err(CoreError::Knowledge)?;
                    self.count(|stats| stats.work_item_lists += 1);
                    truncated |= page.len() > limit as usize;
                    page.truncate(limit as usize);
                    items.extend(page);
                }
                Ok(KnowledgeResult::WorkItems { items, truncated })
            }
            KnowledgeQuery::Lineage(target) => {
                let result = match target {
                    LineageTarget::ProjectPolicy(id) => self
                        .planner
                        .project_store()
                        .policy_lineage(id)
                        .map(KnowledgeResult::PolicyLineage),
                    LineageTarget::UserPolicy(id) => self
                        .planner
                        .global_store()
                        .user_policy_lineage(id)
                        .map(KnowledgeResult::PolicyLineage),
                    LineageTarget::Decision(id) => self
                        .planner
                        .project_store()
                        .decision_lineage(id)
                        .map(KnowledgeResult::DecisionLineage),
                }
                .map_err(CoreError::Knowledge)?;
                self.count(|stats| stats.lineage_reads += 1);
                Ok(result)
            }
            KnowledgeQuery::Handoffs { work_item, limit } => {
                let limit = limit_at_most(limit, MAX_HANDOFF_HISTORY)?;
                let mut handoffs = self
                    .planner
                    .workspace_store()
                    .list_work_handoffs(work_item, limit + 1)
                    .map_err(|error| CoreError::knowledge(error, Some(work_item)))?;
                self.count(|stats| stats.handoff_lists += 1);
                let truncated = handoffs.len() > limit as usize;
                handoffs.truncate(limit as usize);
                Ok(KnowledgeResult::Handoffs {
                    work_item,
                    handoffs,
                    truncated,
                })
            }
        }
    }

    /// The task 9 summary, exactly as it is produced.
    pub fn structure(
        &self,
        request: &StructuralSummaryRequest,
    ) -> Result<StructuralSummary, CoreError> {
        self.check(request.workspace)?;
        let summary = self
            .summary
            .summarize(request)
            .map_err(|error| match error {
                SummaryError::Invalid(error) => {
                    CoreError::InvalidRequest(InvalidRequest::Summary(error))
                }
                other => CoreError::Summary(other),
            })?;
        self.count(|stats| stats.summaries += 1);
        Ok(summary)
    }
}

/// `WorkItemStatus`'s existing closed vocabulary order (`knowledge/model.rs`).
/// `WorkItemStatus` has no `Ord`; this fixed array is Core's own stable
/// order for canonicalizing a caller's status set, without widening the
/// vocabulary's definition.
const STATUS_ORDER: [WorkItemStatus; 6] = [
    WorkItemStatus::Open,
    WorkItemStatus::Active,
    WorkItemStatus::Blocked,
    WorkItemStatus::Paused,
    WorkItemStatus::Completed,
    WorkItemStatus::Abandoned,
];

/// `statuses` deduplicated and put in `STATUS_ORDER`: two requests naming
/// the same set, in any order and with any duplicates, produce the same
/// list here, so their `list_work_items` calls and results are identical.
fn canonical_statuses(statuses: &[WorkItemStatus]) -> Vec<WorkItemStatus> {
    STATUS_ORDER
        .into_iter()
        .filter(|status| statuses.contains(status))
        .collect()
}

/// `limit` as a store limit, or `ListLimitTooLarge`.
fn limit_at_most(limit: NonZeroUsize, max: usize) -> Result<u32, CoreError> {
    if limit.get() > max {
        return Err(CoreError::InvalidRequest(
            InvalidRequest::ListLimitTooLarge {
                limit: limit.get(),
                max,
            },
        ));
    }
    // Every maximum is far below u32::MAX.
    Ok(limit.get() as u32)
}
