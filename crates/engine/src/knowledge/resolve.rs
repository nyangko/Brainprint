//! Deterministic scope / precedence / privacy resolver (#20 task 2).
//!
//! Combines the three task 1 stores for one request without merging them
//! into a generic memory: each category keeps its own type and its own
//! precedence contract (#20 "LOCKED -- Task 2 deterministic precedence
//! boundary"). Nothing here parses natural language, compares free-form
//! text, scores provenance, or reads timestamps to pick a winner. The
//! resolver is read-only.
//!
//! Precedence, per category:
//! - Protected Policy (`PROTECTED_*` + USER_EXPLICIT/AUTHORITATIVE_ARTIFACT
//!   provenance): always kept, outside ordinary precedence. A protected
//!   class on any other provenance is not trusted and is reported as
//!   [`ConflictKind::InvalidProtectedProvenance`].
//! - Keyed ordinary Policy (same `policy_key`): a request directive, then
//!   project.db over global.db, then the most specific context layer. Every
//!   ACTIVE Policy at the winning layer is kept -- Policies are additive.
//! - Unkeyed Policy: kept whenever its exact scope applies.
//! - Decision (same `topic`): a request directive, then the most specific
//!   layer. Different `chosen_summary` values at that layer are a
//!   [`ConflictKind::SameSpecificityDecision`] with no winner.
//! - Preference (same `preference_key`, only keys the request names): a
//!   request directive, then a project Decision on the same topic (even a
//!   conflicted one), then the most specific layer.
//! - Blueprint, Project State, Working State: evidence, never a winner.
//!
//! Result order is presentation only, never precedence: every list is
//! sorted by (subject, specificity layer, origin, content, stable ID); see
//! [`Subject`].

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};

use brainprint_core::{WorkItemId, WorkspaceId};

use super::{
    Blueprint, BlueprintApplication, BlueprintApplicationStatus, BlueprintOwnerKind,
    BlueprintStatus, Decision, DecisionStatus, GlobalKnowledgeStore, KnowledgeError,
    KnowledgeScope, Policy, PolicyStatus, PreferenceStatus, ProjectKnowledgeStore, ProjectState,
    ProjectStateStatus, ProtectionClass, ScopeKind, SourceKind, UserPreference, WorkItem,
    WorkingState, WorkspaceKnowledgeStore,
};

/// Rows fetched per exact scope / subject. Reaching it is an error, never a
/// silent truncation: a truncated protected set would be unsafe.
const FETCH_BOUND: u32 = 512;

#[derive(Debug)]
pub enum ResolveError {
    Knowledge(KnowledgeError),
    /// A malformed [`ApplicabilityContext`]; never normalized.
    InvalidContext(String),
    /// A malformed request (directives, work item, workspace store).
    InvalidRequest(String),
    /// A bounded fetch returned more than [`FETCH_BOUND`] rows.
    BoundExceeded {
        what: &'static str,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Knowledge(source) => write!(formatter, "{source}"),
            Self::InvalidContext(reason) => write!(formatter, "invalid applicability: {reason}"),
            Self::InvalidRequest(reason) => write!(formatter, "invalid resolve request: {reason}"),
            Self::BoundExceeded { what } => {
                write!(
                    formatter,
                    "{what} exceed the resolver bound of {FETCH_BOUND}"
                )
            }
        }
    }
}

impl Error for ResolveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Knowledge(source) => Some(source),
            _ => None,
        }
    }
}

impl From<KnowledgeError> for ResolveError {
    fn from(source: KnowledgeError) -> Self {
        Self::Knowledge(source)
    }
}

// --------------------------------------------------------- applicability

/// Ordered exact scope layers, least specific first. Scopes in one layer
/// are incomparable. The hierarchy is only what the caller declares; it is
/// never inferred from a scope key or a path.
///
/// Rejected, never normalized: an empty layer; a scope appearing twice
/// (in one layer or across layers); GLOBAL or PROJECT sharing a layer,
/// following a narrower layer, or out of GLOBAL -> PROJECT order; more
/// than one WORKSPACE scope. Keyed GLOBAL and non-WorkspaceID WORKSPACE
/// scopes cannot exist ([`KnowledgeScope`] rejects them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicabilityContext {
    layers: Vec<Vec<KnowledgeScope>>,
}

impl ApplicabilityContext {
    pub fn new(layers: Vec<Vec<KnowledgeScope>>) -> Result<Self, ResolveError> {
        let invalid = |reason: &str| Err(ResolveError::InvalidContext(reason.to_owned()));
        let mut seen = BTreeSet::new();
        let mut in_broad_prefix = true;
        let mut previous_broad = None;
        for layer in &layers {
            if layer.is_empty() {
                return invalid("empty specificity layer");
            }
            for scope in layer {
                if !seen.insert(scope) {
                    return invalid("a scope appears more than once");
                }
            }
            let broad = layer
                .iter()
                .find(|scope| matches!(scope.kind(), ScopeKind::Global | ScopeKind::Project));
            match broad {
                Some(_) if layer.len() != 1 => {
                    return invalid("GLOBAL and PROJECT each need a layer of their own");
                }
                Some(_) if !in_broad_prefix => {
                    return invalid("GLOBAL/PROJECT cannot be more specific than a narrower scope");
                }
                Some(scope) => {
                    if previous_broad == Some(ScopeKind::Project)
                        && scope.kind() == ScopeKind::Global
                    {
                        return invalid("GLOBAL must be less specific than PROJECT");
                    }
                    previous_broad = Some(scope.kind());
                }
                None => in_broad_prefix = false,
            }
        }
        if seen
            .iter()
            .filter(|scope| scope.kind() == ScopeKind::Workspace)
            .count()
            > 1
        {
            return invalid("more than one WORKSPACE scope");
        }
        Ok(Self { layers })
    }

    /// `GLOBAL -> PROJECT [-> WORKSPACE(id)]`, the base every request has.
    #[must_use]
    pub fn base(workspace: Option<WorkspaceId>) -> Self {
        let mut layers = vec![
            vec![KnowledgeScope::global()],
            vec![KnowledgeScope::project()],
        ];
        layers.extend(workspace.map(|id| vec![KnowledgeScope::workspace(id)]));
        Self { layers }
    }

    /// Append a more specific layer of exact scopes the caller has evidence
    /// for (package, module, directory, resource, domain, task).
    pub fn with_layer(mut self, scopes: Vec<KnowledgeScope>) -> Result<Self, ResolveError> {
        self.layers.push(scopes);
        Self::new(self.layers)
    }

    #[must_use]
    pub fn layers(&self) -> &[Vec<KnowledgeScope>] {
        &self.layers
    }

    /// Specificity layer of an exact scope, or `None` if it does not apply.
    #[must_use]
    pub fn layer_of(&self, scope: &KnowledgeScope) -> Option<usize> {
        self.layers.iter().position(|layer| layer.contains(scope))
    }

    fn scopes(&self) -> impl Iterator<Item = (usize, &KnowledgeScope)> {
        self.layers
            .iter()
            .enumerate()
            .flat_map(|(index, layer)| layer.iter().map(move |scope| (index, scope)))
    }

    fn workspace(&self) -> Option<&KnowledgeScope> {
        self.scopes()
            .map(|(_, scope)| scope)
            .find(|scope| scope.kind() == ScopeKind::Workspace)
    }
}

// --------------------------------------------------------------- request

/// What a request-local directive overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DirectiveTarget {
    Policy,
    Decision,
    Preference,
}

/// The current explicit instruction as structured input: request-local,
/// never persisted, promoted, or treated as Project truth. Task 2 does not
/// build these from natural language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestDirective {
    /// Request-local identifier; not a durable ID.
    pub id: String,
    pub target: DirectiveTarget,
    /// Exact `policy_key` / Decision `topic` / `preference_key`.
    pub subject_key: String,
    /// Exact scope; must be in the request's context.
    pub scope: KnowledgeScope,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveRequest {
    pub context: ApplicabilityContext,
    pub directives: Vec<RequestDirective>,
    /// Decision topics to resolve. Preference keys are resolved as topics
    /// too, so a project Decision can shadow the Preference.
    pub decision_topics: Vec<String>,
    /// Only these keys are read; none named means no Preference is read.
    pub preference_keys: Vec<String>,
    /// Project / Workspace Project State keys to read as evidence.
    pub state_keys: Vec<String>,
    /// Read applicable Blueprint Applications and their definitions.
    pub include_blueprints: bool,
    /// An explicitly supplied WorkItem; never guessed.
    pub work_item: Option<WorkItemId>,
}

impl ResolveRequest {
    /// Policies only: nothing else is read until the request names it.
    #[must_use]
    pub const fn new(context: ApplicabilityContext) -> Self {
        Self {
            context,
            directives: Vec::new(),
            decision_topics: Vec::new(),
            preference_keys: Vec::new(),
            state_keys: Vec::new(),
            include_blueprints: false,
            work_item: None,
        }
    }

    /// The resolver's own directive checks, without reading any store, so a
    /// projection request (#20 task 5) validates directives by these rules
    /// rather than a copy of them.
    pub(crate) fn check_directives(&self) -> Result<(), ResolveError> {
        directives(self).map(|_| ())
    }
}

/// The stores of one Project and, optionally, the current Workspace.
#[derive(Clone, Copy)]
pub struct KnowledgeSources<'a> {
    pub global: &'a GlobalKnowledgeStore,
    pub project: &'a ProjectKnowledgeStore,
    pub workspace: Option<&'a WorkspaceKnowledgeStore>,
}

// ---------------------------------------------------------------- result

/// Where an item came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    Request,
    Global,
    Project,
    Workspace,
}

/// Canonical, closed reason for every selected, shadowed or evidence item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResolutionReason {
    ProtectedConstraint,
    RequestExplicit,
    UnkeyedPolicy,
    /// Winner of a keyed Policy subject (tier, then specificity).
    SelectedPolicy,
    ResolvedDecision,
    AppliedPreference,
    BlueprintEvidence,
    StateEvidence,
    ShadowedByRequest,
    /// Global user Policy under a same-key project Policy.
    ShadowedByProjectTier,
    ShadowedByMoreSpecificScope,
    /// Global Preference under a same-key project Decision.
    ShadowedByProjectDecision,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Resolved<T> {
    pub item: T,
    pub origin: Origin,
    /// Specificity layer of the item's scope in the request context.
    pub layer: usize,
    pub reason: ResolutionReason,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ShadowedItem {
    Policy(Policy),
    Decision(Decision),
    Preference(UserPreference),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlueprintDefinitionState {
    Available(Box<Blueprint>),
    /// The referenced definition does not exist in its owner store.
    Missing,
    /// The definition exists but is RETIRED; its body is not returned.
    Retired,
}

/// An ACTIVE application plus its definition looked up in its owner store
/// (#20 D2). Design intent evidence, never a precedence tier (D7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlueprintEvidence {
    pub application: BlueprintApplication,
    pub definition: BlueprintDefinitionState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkItemEvidence {
    Found {
        item: Box<WorkItem>,
        working_state: Option<Box<WorkingState>>,
    },
    /// Not in the supplied Workspace's store.
    Missing(WorkItemId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConflictKind {
    /// Different ACTIVE choices for one topic at the winning layer.
    SameSpecificityDecision,
    /// Different ACTIVE values for one preference key at the winning layer.
    SameSpecificityPreference,
    /// A same-key Policy directive against a protected Policy; the
    /// protected Policy stays and the directive is not applied.
    ProtectedOverrideRejected,
    /// A protected class on provenance that cannot carry protected
    /// authority; the row is not applied.
    InvalidProtectedProvenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EvidenceCategory {
    Policy,
    Decision,
    Preference,
    Directive,
}

/// Enough to inspect one side of a conflict later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRef {
    pub category: EvidenceCategory,
    /// Stable ID, or the request-local ID of a directive.
    pub id: String,
    pub origin: Origin,
    pub scope: KnowledgeScope,
    pub layer: usize,
    /// `None` for a directive.
    pub source_kind: Option<SourceKind>,
    /// `None` for a directive.
    pub status: Option<&'static str>,
}

/// A resolution result, never a durable row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeConflict {
    pub kind: ConflictKind,
    pub subject: Option<String>,
    pub involved: Vec<EvidenceRef>,
}

/// Engine-internal resolution result. Not the task 5 projection packet.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResolvedKnowledge {
    /// Directives that were applied (a rejected one is only in `conflicts`).
    pub request_directives: Vec<Resolved<RequestDirective>>,
    pub protected_constraints: Vec<Resolved<Policy>>,
    pub applied_policies: Vec<Resolved<Policy>>,
    pub active_decisions: Vec<Resolved<Decision>>,
    pub applied_preferences: Vec<Resolved<UserPreference>>,
    pub blueprint_evidence: Vec<Resolved<BlueprintEvidence>>,
    pub state_evidence: Vec<Resolved<ProjectState>>,
    pub working_state: Option<WorkItemEvidence>,
    /// Applicable items that lost to another, with the reason.
    pub shadowed: Vec<Resolved<ShadowedItem>>,
    pub conflicts: Vec<KnowledgeConflict>,
}

// ------------------------------------------------------------- ordering

/// Presentation ordering key. Lists sort by (subject, layer, origin,
/// content, stable ID): content before ID so logically identical knowledge
/// orders the same whatever uids it was stored under.
trait Subject {
    fn subject(&self) -> &str;
    fn content(&self) -> String;
    fn id(&self) -> String;
}

impl Subject for Policy {
    fn subject(&self) -> &str {
        self.policy_key.as_deref().unwrap_or_default()
    }
    fn content(&self) -> String {
        format!("{}\n{}", self.title, self.rule_text)
    }
    fn id(&self) -> String {
        self.uid.to_string()
    }
}

impl Subject for Decision {
    fn subject(&self) -> &str {
        &self.topic
    }
    fn content(&self) -> String {
        self.chosen_summary.clone()
    }
    fn id(&self) -> String {
        self.uid.to_string()
    }
}

impl Subject for UserPreference {
    fn subject(&self) -> &str {
        &self.preference_key
    }
    fn content(&self) -> String {
        self.value.to_json()
    }
    fn id(&self) -> String {
        self.uid.to_string()
    }
}

impl Subject for ProjectState {
    fn subject(&self) -> &str {
        &self.key
    }
    fn content(&self) -> String {
        self.value.to_json()
    }
    fn id(&self) -> String {
        self.uid.to_string()
    }
}

impl Subject for BlueprintEvidence {
    fn subject(&self) -> &str {
        ""
    }
    fn content(&self) -> String {
        self.application.application_summary.clone()
    }
    fn id(&self) -> String {
        self.application.uid.to_string()
    }
}

impl Subject for RequestDirective {
    fn subject(&self) -> &str {
        &self.subject_key
    }
    fn content(&self) -> String {
        format!("{:?}\n{}", self.target, self.summary)
    }
    fn id(&self) -> String {
        self.id.clone()
    }
}

impl Subject for ShadowedItem {
    fn subject(&self) -> &str {
        match self {
            Self::Policy(item) => item.subject(),
            Self::Decision(item) => item.subject(),
            Self::Preference(item) => item.subject(),
        }
    }
    fn content(&self) -> String {
        match self {
            Self::Policy(item) => format!("0{}", item.content()),
            Self::Decision(item) => format!("1{}", item.content()),
            Self::Preference(item) => format!("2{}", item.content()),
        }
    }
    fn id(&self) -> String {
        match self {
            Self::Policy(item) => item.id(),
            Self::Decision(item) => item.id(),
            Self::Preference(item) => item.id(),
        }
    }
}

fn sort<T: Subject>(items: &mut [Resolved<T>]) {
    items.sort_by_cached_key(|entry| {
        (
            entry.item.subject().to_owned(),
            entry.layer,
            entry.origin,
            entry.item.content(),
            entry.item.id(),
        )
    });
}

// ------------------------------------------------------------- evidence

fn policy_ref(entry: &Resolved<Policy>) -> EvidenceRef {
    EvidenceRef {
        category: EvidenceCategory::Policy,
        id: entry.item.id(),
        origin: entry.origin,
        scope: entry.item.scope.clone(),
        layer: entry.layer,
        source_kind: Some(entry.item.provenance.source_kind),
        status: Some(entry.item.status.as_str()),
    }
}

fn decision_ref(entry: &Resolved<Decision>) -> EvidenceRef {
    EvidenceRef {
        category: EvidenceCategory::Decision,
        id: entry.item.id(),
        origin: entry.origin,
        scope: entry.item.scope.clone(),
        layer: entry.layer,
        source_kind: Some(entry.item.provenance.source_kind),
        status: Some(entry.item.status.as_str()),
    }
}

fn preference_ref(entry: &Resolved<UserPreference>) -> EvidenceRef {
    EvidenceRef {
        category: EvidenceCategory::Preference,
        id: entry.item.id(),
        origin: entry.origin,
        scope: entry.item.scope.clone(),
        layer: entry.layer,
        source_kind: Some(entry.item.provenance.source_kind),
        status: Some(entry.item.status.as_str()),
    }
}

fn directive_ref(entry: &Resolved<RequestDirective>) -> EvidenceRef {
    EvidenceRef {
        category: EvidenceCategory::Directive,
        id: entry.item.id.clone(),
        origin: Origin::Request,
        scope: entry.item.scope.clone(),
        layer: entry.layer,
        source_kind: None,
        status: None,
    }
}

fn shadow<T>(
    entry: Resolved<T>,
    wrap: fn(T) -> ShadowedItem,
    reason: ResolutionReason,
) -> Resolved<ShadowedItem> {
    Resolved {
        item: wrap(entry.item),
        origin: entry.origin,
        layer: entry.layer,
        reason,
    }
}

const fn at<T>(item: T, origin: Origin, layer: usize, reason: ResolutionReason) -> Resolved<T> {
    Resolved {
        item,
        origin,
        layer,
        reason,
    }
}

fn bounded<T>(
    what: &'static str,
    fetch: impl FnOnce(u32) -> Result<Vec<T>, KnowledgeError>,
) -> Result<Vec<T>, ResolveError> {
    let rows = fetch(FETCH_BOUND + 1)?;
    if rows.len() > FETCH_BOUND as usize {
        return Err(ResolveError::BoundExceeded { what });
    }
    Ok(rows)
}

/// Protected authority needs provenance that can carry it (#20 LOCKED
/// task 2 §6). This is a validity gate, not a provenance rank.
const fn carries_protected_authority(source_kind: SourceKind) -> bool {
    matches!(
        source_kind,
        SourceKind::UserExplicit | SourceKind::AuthoritativeArtifact
    )
}

/// Global user knowledge lives only at GLOBAL / DOMAIN scope.
const fn global_scope(scope: &KnowledgeScope) -> bool {
    matches!(scope.kind(), ScopeKind::Global | ScopeKind::Domain)
}

// -------------------------------------------------------------- resolve

type DirectiveKey = (DirectiveTarget, String);

/// Resolve `request` against `sources`. Read-only.
pub fn resolve(
    sources: &KnowledgeSources<'_>,
    request: &ResolveRequest,
) -> Result<ResolvedKnowledge, ResolveError> {
    let context = &request.context;
    let directives = directives(request)?;
    check_workspace(sources, request)?;
    let mut out = ResolvedKnowledge::default();

    let rejected = resolve_policies(sources, context, &directives, &mut out)?;
    out.request_directives = directives
        .iter()
        .filter(|(key, _)| !rejected.contains(*key))
        .map(|(_, entry)| entry.clone())
        .collect();

    let decided = resolve_decisions(sources, request, &directives, &mut out)?;
    resolve_preferences(sources, request, &directives, &decided, &mut out)?;

    if request.include_blueprints {
        resolve_blueprints(sources, context, &mut out)?;
    }
    resolve_state(sources, request, &mut out)?;
    if let Some(uid) = request.work_item {
        let workspace = sources.workspace.ok_or_else(|| {
            ResolveError::InvalidRequest("a WorkItem needs its Workspace store".to_owned())
        })?;
        out.working_state = Some(match workspace.get_work_item(uid)? {
            Some(item) => WorkItemEvidence::Found {
                working_state: workspace.get_working_state(uid)?.map(Box::new),
                item: Box::new(item),
            },
            None => WorkItemEvidence::Missing(uid),
        });
    }

    sort(&mut out.request_directives);
    sort(&mut out.protected_constraints);
    sort(&mut out.applied_policies);
    sort(&mut out.active_decisions);
    sort(&mut out.applied_preferences);
    sort(&mut out.blueprint_evidence);
    sort(&mut out.state_evidence);
    sort(&mut out.shadowed);
    for conflict in &mut out.conflicts {
        conflict
            .involved
            .sort_by(|a, b| (a.category, &a.id).cmp(&(b.category, &b.id)));
    }
    out.conflicts.sort_by(|a, b| {
        let ids = |conflict: &KnowledgeConflict| {
            conflict
                .involved
                .iter()
                .map(|evidence| evidence.id.clone())
                .collect::<Vec<_>>()
        };
        (a.kind, &a.subject, ids(a)).cmp(&(b.kind, &b.subject, ids(b)))
    });
    Ok(out)
}

fn directives(
    request: &ResolveRequest,
) -> Result<BTreeMap<DirectiveKey, Resolved<RequestDirective>>, ResolveError> {
    let invalid = |reason: &str| ResolveError::InvalidRequest(reason.to_owned());
    let mut ids = BTreeSet::new();
    let mut out = BTreeMap::new();
    for directive in &request.directives {
        if directive.id.is_empty() || directive.subject_key.is_empty() {
            return Err(invalid("a directive needs an id and a subject key"));
        }
        if !ids.insert(directive.id.as_str()) {
            return Err(invalid("duplicate directive id"));
        }
        let layer = request
            .context
            .layer_of(&directive.scope)
            .ok_or_else(|| invalid("directive scope is not in the applicability context"))?;
        let key = (directive.target, directive.subject_key.clone());
        let entry = at(
            directive.clone(),
            Origin::Request,
            layer,
            ResolutionReason::RequestExplicit,
        );
        if out.insert(key, entry).is_some() {
            return Err(invalid("two directives for one target and subject"));
        }
    }
    Ok(out)
}

/// A supplied Workspace store must belong to the context's Workspace, so
/// Workspace A's state can never answer for Workspace B.
fn check_workspace(
    sources: &KnowledgeSources<'_>,
    request: &ResolveRequest,
) -> Result<(), ResolveError> {
    let (Some(store), Some(scope)) = (sources.workspace, request.context.workspace()) else {
        return Ok(());
    };
    match store.bound_workspace_id()? {
        Some(bound) if Some(bound.to_string().as_str()) != scope.key() => Err(
            ResolveError::InvalidRequest("Workspace store belongs to another Workspace".to_owned()),
        ),
        _ => Ok(()),
    }
}

/// Returns the Policy directives rejected by a protected constraint.
fn resolve_policies(
    sources: &KnowledgeSources<'_>,
    context: &ApplicabilityContext,
    directives: &BTreeMap<DirectiveKey, Resolved<RequestDirective>>,
    out: &mut ResolvedKnowledge,
) -> Result<BTreeSet<DirectiveKey>, ResolveError> {
    // Exact scopes of the context only (task 1 access path P2 / G2).
    let mut loaded = Vec::new();
    for (layer, scope) in context.scopes() {
        if global_scope(scope) {
            for policy in bounded("user policies of one scope", |limit| {
                sources
                    .global
                    .list_user_policies(scope, PolicyStatus::Active, limit)
            })? {
                loaded.push((policy, Origin::Global, layer));
            }
        }
        if scope.kind() != ScopeKind::Global {
            for policy in bounded("project policies of one scope", |limit| {
                sources
                    .project
                    .list_policies(scope, PolicyStatus::Active, limit)
            })? {
                loaded.push((policy, Origin::Project, layer));
            }
        }
    }

    let mut keyed: BTreeMap<String, Vec<Resolved<Policy>>> = BTreeMap::new();
    for (policy, origin, layer) in loaded {
        let protected = policy.protection_class != ProtectionClass::Normal;
        let key = policy.policy_key.clone();
        let entry = at(policy, origin, layer, ResolutionReason::ProtectedConstraint);
        if protected {
            if carries_protected_authority(entry.item.provenance.source_kind) {
                out.protected_constraints.push(entry);
            } else {
                out.conflicts.push(KnowledgeConflict {
                    kind: ConflictKind::InvalidProtectedProvenance,
                    subject: key,
                    involved: vec![policy_ref(&entry)],
                });
            }
            continue;
        }
        match key {
            None => out.applied_policies.push(Resolved {
                reason: ResolutionReason::UnkeyedPolicy,
                ..entry
            }),
            Some(key) => keyed.entry(key).or_default().push(entry),
        }
    }

    let mut rejected = BTreeSet::new();
    for (key, directive) in directives {
        if key.0 != DirectiveTarget::Policy {
            continue;
        }
        let guarded: Vec<_> = out
            .protected_constraints
            .iter()
            .filter(|entry| entry.item.policy_key.as_deref() == Some(key.1.as_str()))
            .map(policy_ref)
            .collect();
        if !guarded.is_empty() {
            rejected.insert(key.clone());
            let mut involved = vec![directive_ref(directive)];
            involved.extend(guarded);
            out.conflicts.push(KnowledgeConflict {
                kind: ConflictKind::ProtectedOverrideRejected,
                subject: Some(key.1.clone()),
                involved,
            });
        }
    }

    for (key, candidates) in keyed {
        let directive_key = (DirectiveTarget::Policy, key);
        if directives.contains_key(&directive_key) && !rejected.contains(&directive_key) {
            out.shadowed.extend(candidates.into_iter().map(|entry| {
                shadow(
                    entry,
                    ShadowedItem::Policy,
                    ResolutionReason::ShadowedByRequest,
                )
            }));
            continue;
        }
        let project_tier = candidates
            .iter()
            .any(|entry| entry.origin == Origin::Project);
        let tier = |entry: &Resolved<Policy>| !project_tier || entry.origin == Origin::Project;
        let top = candidates
            .iter()
            .filter(|entry| tier(entry))
            .map(|entry| entry.layer)
            .max();
        for entry in candidates {
            let reason = if !tier(&entry) {
                ResolutionReason::ShadowedByProjectTier
            } else if Some(entry.layer) != top {
                ResolutionReason::ShadowedByMoreSpecificScope
            } else {
                out.applied_policies.push(Resolved {
                    reason: ResolutionReason::SelectedPolicy,
                    ..entry
                });
                continue;
            };
            out.shadowed
                .push(shadow(entry, ShadowedItem::Policy, reason));
        }
    }
    Ok(rejected)
}

/// Returns the topics that have an applicable ACTIVE project Decision
/// (resolved or conflicted).
fn resolve_decisions(
    sources: &KnowledgeSources<'_>,
    request: &ResolveRequest,
    directives: &BTreeMap<DirectiveKey, Resolved<RequestDirective>>,
    out: &mut ResolvedKnowledge,
) -> Result<BTreeSet<String>, ResolveError> {
    let topics: BTreeSet<&String> = request
        .decision_topics
        .iter()
        .chain(&request.preference_keys)
        .collect();
    let mut decided = BTreeSet::new();
    for topic in topics {
        // Task 1 access path P4 (topic + status); a topic holds a few rows,
        // so dropping inapplicable scopes in memory stays bounded.
        let rows: Vec<_> = bounded("decisions of one topic", |limit| {
            sources
                .project
                .list_decisions_by_topic(topic, DecisionStatus::Active, limit)
        })?
        .into_iter()
        .filter_map(|decision| {
            let layer = request.context.layer_of(&decision.scope)?;
            Some(at(
                decision,
                Origin::Project,
                layer,
                ResolutionReason::ResolvedDecision,
            ))
        })
        .collect();
        if rows.is_empty() {
            continue;
        }
        decided.insert(topic.clone());
        if directives.contains_key(&(DirectiveTarget::Decision, topic.clone())) {
            out.shadowed.extend(rows.into_iter().map(|entry| {
                shadow(
                    entry,
                    ShadowedItem::Decision,
                    ResolutionReason::ShadowedByRequest,
                )
            }));
            continue;
        }
        let top = rows.iter().map(|entry| entry.layer).max();
        let (winners, lower): (Vec<_>, Vec<_>) =
            rows.into_iter().partition(|entry| Some(entry.layer) == top);
        out.shadowed.extend(lower.into_iter().map(|entry| {
            shadow(
                entry,
                ShadowedItem::Decision,
                ResolutionReason::ShadowedByMoreSpecificScope,
            )
        }));
        let choices: BTreeSet<&str> = winners
            .iter()
            .map(|entry| entry.item.chosen_summary.as_str())
            .collect();
        if choices.len() == 1 {
            out.active_decisions.extend(winners);
        } else {
            out.conflicts.push(KnowledgeConflict {
                kind: ConflictKind::SameSpecificityDecision,
                subject: Some(topic.clone()),
                involved: winners.iter().map(decision_ref).collect(),
            });
        }
    }
    Ok(decided)
}

fn resolve_preferences(
    sources: &KnowledgeSources<'_>,
    request: &ResolveRequest,
    directives: &BTreeMap<DirectiveKey, Resolved<RequestDirective>>,
    decided: &BTreeSet<String>,
    out: &mut ResolvedKnowledge,
) -> Result<(), ResolveError> {
    let keys: BTreeSet<&String> = request.preference_keys.iter().collect();
    for key in keys {
        // Task 1 access path G4: exact scope + key + status.
        let mut rows = Vec::new();
        for (layer, scope) in request.context.scopes().filter(|(_, s)| global_scope(s)) {
            for preference in bounded("preferences of one key", |limit| {
                sources
                    .global
                    .find_user_preferences(scope, key, PreferenceStatus::Active, limit)
            })? {
                rows.push(at(
                    preference,
                    Origin::Global,
                    layer,
                    ResolutionReason::AppliedPreference,
                ));
            }
        }
        let overridden = if directives.contains_key(&(DirectiveTarget::Preference, key.clone()))
            || directives.contains_key(&(DirectiveTarget::Decision, key.clone()))
        {
            Some(ResolutionReason::ShadowedByRequest)
        } else if decided.contains(key) {
            Some(ResolutionReason::ShadowedByProjectDecision)
        } else {
            None
        };
        if let Some(reason) = overridden {
            out.shadowed.extend(
                rows.into_iter()
                    .map(|entry| shadow(entry, ShadowedItem::Preference, reason)),
            );
            continue;
        }
        let top = rows.iter().map(|entry| entry.layer).max();
        let (winners, lower): (Vec<_>, Vec<_>) =
            rows.into_iter().partition(|entry| Some(entry.layer) == top);
        out.shadowed.extend(lower.into_iter().map(|entry| {
            shadow(
                entry,
                ShadowedItem::Preference,
                ResolutionReason::ShadowedByMoreSpecificScope,
            )
        }));
        let values: BTreeSet<String> = winners.iter().map(|entry| entry.item.content()).collect();
        if values.len() > 1 {
            out.conflicts.push(KnowledgeConflict {
                kind: ConflictKind::SameSpecificityPreference,
                subject: Some(key.clone()),
                involved: winners.iter().map(preference_ref).collect(),
            });
        } else {
            out.applied_preferences.extend(winners);
        }
    }
    Ok(())
}

fn resolve_blueprints(
    sources: &KnowledgeSources<'_>,
    context: &ApplicabilityContext,
    out: &mut ResolvedKnowledge,
) -> Result<(), ResolveError> {
    for (layer, scope) in context.scopes() {
        if scope.kind() == ScopeKind::Global {
            continue;
        }
        // Task 1 access path B3, then B1 / G6 by uid in the owner store.
        for application in bounded("blueprint applications of one scope", |limit| {
            sources.project.list_blueprint_applications(
                scope,
                BlueprintApplicationStatus::Active,
                limit,
            )
        })? {
            let definition = match application.blueprint.owner {
                BlueprintOwnerKind::Global => {
                    sources.global.get_blueprint(application.blueprint.uid)
                }
                BlueprintOwnerKind::Project => {
                    sources.project.get_blueprint(application.blueprint.uid)
                }
            }?;
            let definition = match definition {
                None => BlueprintDefinitionState::Missing,
                Some(blueprint) if blueprint.status == BlueprintStatus::Retired => {
                    BlueprintDefinitionState::Retired
                }
                Some(blueprint) => BlueprintDefinitionState::Available(Box::new(blueprint)),
            };
            out.blueprint_evidence.push(at(
                BlueprintEvidence {
                    application,
                    definition,
                },
                Origin::Project,
                layer,
                ResolutionReason::BlueprintEvidence,
            ));
        }
    }
    Ok(())
}

fn resolve_state(
    sources: &KnowledgeSources<'_>,
    request: &ResolveRequest,
    out: &mut ResolvedKnowledge,
) -> Result<(), ResolveError> {
    let keys: BTreeSet<&String> = request.state_keys.iter().collect();
    for key in keys {
        for (layer, scope) in request.context.scopes() {
            if scope.kind() == ScopeKind::Global {
                continue;
            }
            // Task 1 access paths S1 / W9: exact key + scope.
            let project = sources.project.get_project_state(key, scope)?;
            let workspace = match sources.workspace {
                Some(store) => store.get_workspace_project_state(key, scope)?,
                None => None,
            };
            for (state, origin) in [(project, Origin::Project), (workspace, Origin::Workspace)] {
                if let Some(state) = state.filter(|s| s.status == ProjectStateStatus::Current) {
                    out.state_evidence.push(at(
                        state,
                        origin,
                        layer,
                        ResolutionReason::StateEvidence,
                    ));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
