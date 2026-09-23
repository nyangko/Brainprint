//! Transport-neutral projection request and evidence contract (#20 task 5).
//!
//! Vocabulary only. Nothing here reads a store, a source file, or the
//! graph: choosing evidence is task 6's planner, budgeting and
//! continuation are task 7's, delivery accounting is task 8's, and every
//! transport (CLI, daemon, MCP) maps onto these types later.
//!
//! ## Request
//!
//! A [`ProjectionRequest`] states *intent* and a structured *target* in a
//! mandatory Workspace. It carries no capability switches and no
//! natural-language dump: which evidence an intent needs is the planner's
//! decision. The Project is derived from the Workspace binding, never
//! supplied beside it, and current generation/revision is read by the
//! runtime, never required from the caller.
//!
//! ## Evidence
//!
//! An [`EvidenceItem`] wraps one existing canonical fact. Its
//! [`EvidenceOrigin`] is a function of the payload kind, so a stored Policy
//! cannot be relabelled OBSERVED. Mixed-origin composites
//! (`PreparedInspection`, `ResolvedKnowledge`, `WorkSnapshot`) have no
//! variant: the planner decomposes them into their constituent facts.

use std::{collections::BTreeSet, error::Error, fmt};

use brainprint_core::{ResourceId, SymbolId, WorkItemId, WorkspaceId};

use crate::{
    coverage::{AnswerState, CoverageReport},
    graph::{GraphEndpoint, RelationKind},
    impact::ImpactIntent,
    knowledge::{
        ApplicabilityContext, BlueprintEvidence, Decision, GenerationReference, KnowledgeConflict,
        KnowledgeScope, Policy, ProjectState, RequestDirective, ResolveError, ResolveRequest,
        Resolved, Staleness, UserPreference, WorkHandoff, WorkItem, WorkOverlap, WorkResult,
        WorkingState,
    },
    parser::SourceSpan,
    prepare::{PreparedRange, SourceUnavailable},
    query::{
        Currentness, Located, ResourceLocator, ResourceScope, SymbolCandidate, SymbolQuery,
        SymbolSelector,
    },
    related_tests::RelatedTestCandidate,
    relations::{Direction, RelationGap, RelationResult},
    resource::{Resource, ResourceLanguage},
    symbol::SymbolKind,
};

// ---------------------------------------------------------------- intent

/// What the caller is trying to do. Task meanings, not tool names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionIntent {
    Locate,
    Understand,
    /// An edit. `None` is a plain edit with no declared change form.
    Change(Option<ChangeKind>),
    /// What a change would affect; the change form is required.
    Impact(ChangeKind),
    /// Resume or hand off an explicitly named WorkItem.
    ResumeHandoff,
}

/// The form of a change (#6 task 5 §1). Declared by the caller, executed
/// by nothing here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// Signature, rename, module move, base/interface: I3's existing
    /// closed vocabulary, reused rather than restated.
    Structural(ImpactIntent),
    Delete,
    /// A route/env/config/table/cache key contract change. One family,
    /// not one entity kind: each keeps its own domain identity.
    DomainContractChange,
}

// ---------------------------------------------------------------- target

/// What the request is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionTarget {
    /// An exact graph identity, kept verbatim.
    Endpoint(GraphEndpoint),
    Resource(ResourceTarget),
    Symbol(SymbolTarget),
}

/// Owned form of [`ResourceLocator`], with the same meaning per variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceTarget {
    Id(ResourceId),
    /// Full Workspace-relative path: exact.
    Path(String),
    /// Final path segment: a search, possibly several Resources.
    Basename(String),
    /// Every Resource under the prefix: a search.
    PathPrefix(String),
}

impl ResourceTarget {
    /// The structured query selector this names.
    #[must_use]
    pub fn locator(&self) -> ResourceLocator<'_> {
        match self {
            Self::Id(id) => ResourceLocator::Id(*id),
            Self::Path(path) => ResourceLocator::Path(path),
            Self::Basename(name) => ResourceLocator::Basename(name),
            Self::PathPrefix(prefix) => ResourceLocator::PathPrefix(prefix),
        }
    }

    fn text(&self) -> Option<&str> {
        match self {
            Self::Id(_) => None,
            Self::Path(text) | Self::Basename(text) | Self::PathPrefix(text) => Some(text),
        }
    }
}

/// Owned form of [`SymbolSelector`], with the same meaning per variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolName {
    Id(SymbolId),
    QualifiedName(String),
    /// Not unique: every declaration with this name.
    Name(String),
    /// Substring search.
    PartialName(String),
}

/// Owned form of [`SymbolQuery`] without a limit: the Resource scope is an
/// exact Resource only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolTarget {
    pub name: SymbolName,
    pub resource: Option<ResourceId>,
    pub kind: Option<SymbolKind>,
    pub language: Option<ResourceLanguage>,
}

impl SymbolTarget {
    #[must_use]
    pub const fn new(name: SymbolName) -> Self {
        Self {
            name,
            resource: None,
            kind: None,
            language: None,
        }
    }

    /// The structured query this names, at the query's own default
    /// candidate limit.
    #[must_use]
    pub fn query(&self) -> SymbolQuery<'_> {
        SymbolQuery {
            selector: match &self.name {
                SymbolName::Id(id) => SymbolSelector::Id(*id),
                SymbolName::QualifiedName(name) => SymbolSelector::QualifiedName(name),
                SymbolName::Name(name) => SymbolSelector::Name(name),
                SymbolName::PartialName(name) => SymbolSelector::PartialName(name),
            },
            scope: self.resource.map(ResourceScope::Id),
            kind: self.kind,
            language: self.language,
            limit: None,
        }
    }

    fn text(&self) -> Option<&str> {
        match &self.name {
            SymbolName::Id(_) => None,
            SymbolName::QualifiedName(text)
            | SymbolName::Name(text)
            | SymbolName::PartialName(text) => Some(text),
        }
    }
}

// ----------------------------------------------------------- correlation

/// Optional, opaque client/orchestrator context for later role-aware
/// projection and delivery accounting.
///
/// Never Project/Workspace identity, authority, permission, Policy, or
/// ownership. Role and persona may only steer selection/presentation
/// later; they never change Resource/Symbol identity, Relation existence,
/// revision, freshness, or Policy/Decision truth. Absent entirely, every
/// capability still works.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectionCorrelation {
    pub client_id: Option<String>,
    pub session_id: Option<String>,
    pub external_task_id: Option<String>,
    pub external_subtask_id: Option<String>,
    pub role_hint: Option<String>,
    pub team_hint: Option<String>,
    /// Compact traits; a set, so supply order means nothing.
    pub persona_traits: BTreeSet<String>,
}

/// An opaque identifier field of [`ProjectionCorrelation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrelationField {
    ClientId,
    SessionId,
    ExternalTaskId,
    ExternalSubtaskId,
}

impl ProjectionCorrelation {
    fn check(&self) -> Result<(), ProjectionRequestError> {
        let ids = [
            (CorrelationField::ClientId, &self.client_id),
            (CorrelationField::SessionId, &self.session_id),
            (CorrelationField::ExternalTaskId, &self.external_task_id),
            (
                CorrelationField::ExternalSubtaskId,
                &self.external_subtask_id,
            ),
        ];
        match ids
            .into_iter()
            .find(|(_, id)| id.as_deref().is_some_and(str::is_empty))
        {
            Some((field, _)) => Err(ProjectionRequestError::EmptyCorrelationId(field)),
            None => Ok(()),
        }
    }
}

// --------------------------------------------------------------- request

/// One projection request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionRequest {
    pub workspace: WorkspaceId,
    pub intent: ProjectionIntent,
    pub target: Option<ProjectionTarget>,
    /// Explicit only; never guessed or taken as "the latest active one".
    pub work_item: Option<WorkItemId>,
    /// Exact scope layers the caller has evidence for, beyond
    /// `GLOBAL -> PROJECT -> WORKSPACE`, least specific first. Never
    /// inferred from a path or a name.
    pub scope_layers: Vec<Vec<KnowledgeScope>>,
    /// Request-local current instructions; never persisted or promoted.
    pub directives: Vec<RequestDirective>,
    pub correlation: Option<ProjectionCorrelation>,
}

impl ProjectionRequest {
    #[must_use]
    pub const fn new(workspace: WorkspaceId, intent: ProjectionIntent) -> Self {
        Self {
            workspace,
            intent,
            target: None,
            work_item: None,
            scope_layers: Vec::new(),
            directives: Vec::new(),
            correlation: None,
        }
    }

    /// Structural validation. Returns the effective applicability: the
    /// Workspace base plus the supplied layers, built and checked by the
    /// resolver's own [`ApplicabilityContext`].
    pub fn validate(&self) -> Result<ApplicabilityContext, ProjectionRequestError> {
        match (&self.intent, &self.target, self.work_item) {
            (ProjectionIntent::ResumeHandoff, _, None) => {
                return Err(ProjectionRequestError::MissingWorkItem);
            }
            (ProjectionIntent::ResumeHandoff, _, Some(_)) | (_, Some(_), _) => {}
            (_, None, _) => return Err(ProjectionRequestError::MissingTarget),
        }
        let text = match &self.target {
            Some(ProjectionTarget::Resource(target)) => target.text(),
            Some(ProjectionTarget::Symbol(target)) => target.text(),
            Some(ProjectionTarget::Endpoint(_)) | None => None,
        };
        if text.is_some_and(str::is_empty) {
            return Err(ProjectionRequestError::EmptySelector);
        }

        let mut context = ApplicabilityContext::base(Some(self.workspace));
        for layer in &self.scope_layers {
            context = context
                .with_layer(layer.clone())
                .map_err(ProjectionRequestError::InvalidScopeLayer)?;
        }
        if let Some(outside) = self
            .directives
            .iter()
            .find(|directive| context.layer_of(&directive.scope).is_none())
        {
            return Err(ProjectionRequestError::DirectiveOutsideApplicability {
                directive_id: outside.id.clone(),
            });
        }
        let mut check = ResolveRequest::new(context);
        check.directives.clone_from(&self.directives);
        check
            .check_directives()
            .map_err(ProjectionRequestError::InvalidDirective)?;

        if let Some(correlation) = &self.correlation {
            correlation.check()?;
        }
        Ok(check.context)
    }
}

/// A structurally invalid request.
#[derive(Debug)]
pub enum ProjectionRequestError {
    /// LOCATE / UNDERSTAND / CHANGE / IMPACT without a target.
    MissingTarget,
    /// RESUME_HANDOFF without an explicit WorkItem.
    MissingWorkItem,
    /// A path, basename, prefix, or name selector that is empty.
    EmptySelector,
    /// An extra scope layer the applicability rules reject.
    InvalidScopeLayer(ResolveError),
    /// A directive whose scope is not in the request's applicability.
    DirectiveOutsideApplicability {
        directive_id: String,
    },
    /// A directive the resolver's rules reject (empty id/subject,
    /// duplicate id, two for one subject).
    InvalidDirective(ResolveError),
    EmptyCorrelationId(CorrelationField),
}

impl fmt::Display for ProjectionRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingTarget => formatter.write_str("this intent requires a target"),
            Self::MissingWorkItem => formatter.write_str("resume/handoff requires a WorkItem"),
            Self::EmptySelector => formatter.write_str("target selector is empty"),
            Self::InvalidScopeLayer(source) => write!(formatter, "invalid scope layer: {source}"),
            Self::DirectiveOutsideApplicability { directive_id } => write!(
                formatter,
                "directive {directive_id} is outside the request applicability"
            ),
            Self::InvalidDirective(source) => write!(formatter, "invalid directive: {source}"),
            Self::EmptyCorrelationId(field) => {
                write!(formatter, "correlation {field:?} is empty")
            }
        }
    }
}

impl Error for ProjectionRequestError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidScopeLayer(source) | Self::InvalidDirective(source) => Some(source),
            _ => None,
        }
    }
}

// -------------------------------------------------------------- evidence

/// How Brainprint holds a fact for this packet (#8 Fact-only Truth
/// Surface). Closed: there is no estimated, inferred, predicted, or
/// recommended origin, and no confidence.
///
/// Orthogonal to category provenance: a Policy is STORED here and keeps
/// its own `Provenance` (e.g. USER_EXPLICIT) for why it exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EvidenceOrigin {
    /// Current filesystem/tool/request observation.
    Observed,
    /// Canonical persisted Brainprint truth.
    Stored,
    /// Deterministic computation over observed/stored facts.
    Derived,
}

/// Which stored generation reference of a WorkItem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationBasis {
    Baseline,
    Result,
}

/// How a target selector resolved, without choosing among candidates.
/// [`Located::exact`] is the only single answer, and only for an exact
/// selector with one match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetSelection {
    pub selector: ProjectionTarget,
    pub located: Located<GraphEndpoint>,
}

/// What a coverage report is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverageSubject {
    TargetSelection(ProjectionTarget),
    Relations {
        anchor: GraphEndpoint,
        direction: Direction,
        /// Empty means every kind.
        kinds: Vec<RelationKind>,
    },
    Impact {
        root: GraphEndpoint,
        intent: ImpactIntent,
    },
    RelatedTests {
        target: GraphEndpoint,
        intent: ImpactIntent,
    },
}

/// A result family's coverage: the existing limits plus the confirmed
/// count, from which the three-way answer state follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageEvidence {
    pub subject: CoverageSubject,
    pub report: CoverageReport,
    pub confirmed: usize,
}

impl CoverageEvidence {
    #[must_use]
    pub fn answer_state(&self) -> AnswerState {
        self.report.state(self.confirmed)
    }
}

/// One fact a packet may carry. The variant decides the origin.
///
/// Source text lives only in [`Self::CurrentSource`]; a relation, gap, or
/// test carries locators (`ResourceId` + basis revision + span) that match
/// a source range by those semantic identities, not by a result-local
/// `RangeId`.
#[derive(Debug, Clone, PartialEq)]
pub enum EvidenceItem {
    // STORED
    Resource(Resource),
    Symbol(SymbolCandidate),
    Relation(RelationResult),
    RelationGap(RelationGap),
    Policy(Resolved<Policy>),
    Decision(Resolved<Decision>),
    Preference(Resolved<UserPreference>),
    Blueprint(Resolved<BlueprintEvidence>),
    ProjectState(Resolved<ProjectState>),
    WorkItem(WorkItem),
    WorkingState(WorkingState),
    WorkResult(WorkResult),
    Handoff(WorkHandoff),

    // OBSERVED
    /// A verified current source range.
    CurrentSource(PreparedRange),
    Directive(Resolved<RequestDirective>),

    // DERIVED
    RelatedTest {
        target: GraphEndpoint,
        candidate: RelatedTestCandidate,
    },
    KnowledgeConflict(KnowledgeConflict),
    WorkOverlap {
        work_item: WorkItemId,
        overlap: WorkOverlap,
    },
    GenerationReference {
        work_item: WorkItemId,
        basis: GenerationBasis,
        reference: GenerationReference,
    },
    WorkStaleness {
        work_item: WorkItemId,
        staleness: Staleness,
    },
    TargetSelection(TargetSelection),
    Coverage(CoverageEvidence),
    SourceUnavailable {
        resource: ResourceId,
        span: SourceSpan,
        reason: SourceUnavailable,
    },
    /// Whether the Workspace's structural index may be claimed current.
    IndexCurrentness {
        workspace: WorkspaceId,
        currentness: Currentness,
    },
}

impl EvidenceItem {
    #[must_use]
    pub const fn origin(&self) -> EvidenceOrigin {
        match self {
            Self::Resource(_)
            | Self::Symbol(_)
            | Self::Relation(_)
            | Self::RelationGap(_)
            | Self::Policy(_)
            | Self::Decision(_)
            | Self::Preference(_)
            | Self::Blueprint(_)
            | Self::ProjectState(_)
            | Self::WorkItem(_)
            | Self::WorkingState(_)
            | Self::WorkResult(_)
            | Self::Handoff(_) => EvidenceOrigin::Stored,
            Self::CurrentSource(_) | Self::Directive(_) => EvidenceOrigin::Observed,
            Self::RelatedTest { .. }
            | Self::KnowledgeConflict(_)
            | Self::WorkOverlap { .. }
            | Self::GenerationReference { .. }
            | Self::WorkStaleness { .. }
            | Self::TargetSelection(_)
            | Self::Coverage(_)
            | Self::SourceUnavailable { .. }
            | Self::IndexCurrentness { .. } => EvidenceOrigin::Derived,
        }
    }
}

#[cfg(test)]
mod tests;
