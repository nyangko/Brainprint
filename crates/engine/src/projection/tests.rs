//! #20 task 5 acceptance: request validation, owned selector conversion,
//! evidence origin mapping, and the contract-only boundary. No store,
//! source read, resolver run, or semantic backend is needed.

use std::collections::BTreeSet;

use brainprint_core::{
    DecisionId, LogicalSymbolId, PolicyId, ResourceId, SymbolId, WorkItemId, WorkspaceId,
};

use super::*;
use crate::{
    coverage::{AnswerState, CoverageLimit, CoverageReport},
    gaps::{IntendedRelation, UnresolvedReason},
    graph::{DomainEntity, RelationKind},
    impact::ImpactIntent,
    inspect::SourceVerification,
    knowledge::DirtyObservation,
    knowledge::{
        ConflictKind, DirectiveTarget, EvidenceCategory, EvidenceRef, KnowledgeConflict,
        KnowledgeScope, Origin, Policy, PolicyStatus, PriorityClass, ProtectionClass, Provenance,
        RequestDirective, ResolutionReason, ResolveError, Resolved, ScopeKind, SourceKind,
        WorkItemStatus, WorkOverlap, WorkResourceRole, WorkingState,
    },
    parser::{SourcePoint, SourceSpan},
    prepare::{PreparedRange, RangeRole, SourceUnavailable},
    query::{
        Currentness, Located, NotCurrentReason, ResourceLocator, ResourceScope, ResultSource,
        StructuralCoverage, SymbolCandidate, SymbolSelector,
    },
    related_tests::{ProjectionBasis, RelatedTestCandidate},
    relations::{Direction, EvidenceLocation, RelationGap, RelationResult},
    resolution::{Dispatch, Freshness, Resolution, Support, TargetScope},
    resource::{Resource, ResourceKind, ResourceLanguage, ResourceRole, ResourceState},
    schema,
    symbol::{OccurrenceKind, Symbol, SymbolKind, Visibility},
};

// --------------------------------------------------------------- fixtures

fn span() -> SourceSpan {
    SourceSpan {
        start_byte: 10,
        end_byte: 42,
        start: SourcePoint { line: 1, column: 0 },
        end: SourcePoint { line: 3, column: 1 },
    }
}

fn symbol_target(name: SymbolName) -> ProjectionTarget {
    ProjectionTarget::Symbol(SymbolTarget::new(name))
}

fn request(intent: ProjectionIntent) -> ProjectionRequest {
    let mut request = ProjectionRequest::new(WorkspaceId::generate(), intent);
    request.target = Some(ProjectionTarget::Resource(ResourceTarget::Path(
        "src/auth.rs".to_owned(),
    )));
    request
}

fn directive(id: &str, scope: KnowledgeScope) -> RequestDirective {
    RequestDirective {
        id: id.to_owned(),
        target: DirectiveTarget::Policy,
        subject_key: "style".to_owned(),
        scope,
        summary: "use tabs".to_owned(),
    }
}

fn resolved<T>(item: T) -> Resolved<T> {
    Resolved {
        item,
        origin: Origin::Project,
        layer: 1,
        reason: ResolutionReason::SelectedPolicy,
    }
}

fn location(resource: ResourceId) -> EvidenceLocation {
    EvidenceLocation {
        resource,
        containing_symbol: None,
        occurrence_kind: OccurrenceKind::CallSite,
        span: span(),
        basis_revision: "rev-1".to_owned(),
        support: Support::Supported,
        freshness: Freshness::Fresh,
    }
}

fn relation() -> RelationResult {
    let resource = ResourceId::generate();
    RelationResult {
        kind: RelationKind::Calls,
        source: GraphEndpoint::Resource(resource),
        target: GraphEndpoint::Symbol(SymbolId::generate()),
        direction: Direction::Outgoing,
        dispatch: Dispatch::Static,
        target_scope: TargetScope::Internal,
        resolution: Resolution::Resolved,
        support: Support::Supported,
        freshness: Freshness::Fresh,
        evidence: vec![location(resource)],
    }
}

fn gap(candidates: Vec<GraphEndpoint>, truncated: bool) -> RelationGap {
    RelationGap {
        location: location(ResourceId::generate()),
        intended: IntendedRelation::Known(RelationKind::Imports),
        lookup_name: "./missing".to_owned(),
        module_hint: None,
        reason: UnresolvedReason::MissingRelativeTarget,
        resolution: if candidates.is_empty() {
            Resolution::Unresolved
        } else {
            Resolution::Candidate
        },
        candidates,
        candidate_truncated: truncated,
        resolution_context_key: None,
    }
}

fn policy(source_kind: SourceKind) -> Policy {
    Policy {
        uid: PolicyId::generate(),
        scope: KnowledgeScope::project(),
        policy_key: Some("style".to_owned()),
        title: "Style".to_owned(),
        rule_text: "use spaces".to_owned(),
        structured_rule: None,
        protection_class: ProtectionClass::Normal,
        priority_class: PriorityClass::Default,
        status: PolicyStatus::Active,
        provenance: Provenance::new(source_kind).with_locator("issue#7"),
        created_at: "t0".to_owned(),
        updated_at: "t0".to_owned(),
    }
}

fn working_state() -> WorkingState {
    WorkingState {
        work_item: WorkItemId::generate(),
        baseline_workspace_revision: "ws-1".to_owned(),
        baseline_index_incarnation: None,
        baseline_generation_no: 3,
        baseline_head: None,
        baseline_dirty: DirtyObservation::Clean,
        current_step: None,
        progress_summary: None,
        remaining_summary: None,
        blocker_summary: None,
        owner_agent: None,
        last_observed_workspace_revision: "ws-1".to_owned(),
        updated_at: "t0".to_owned(),
    }
}

fn resource() -> Resource {
    Resource {
        id: ResourceId::generate(),
        path_rel: "src/auth.rs".to_owned(),
        path_key: "src/auth.rs".to_owned(),
        kind: ResourceKind::File,
        role: ResourceRole::Source,
        language: Some(ResourceLanguage::Rust),
        size_bytes: 100,
        mtime_ns: 0,
        fingerprint: "fp".to_owned(),
        content_hash: Some("h".to_owned()),
        state: ResourceState::Active,
        resource_revision: "rev-1".to_owned(),
        generated_kind: None,
        container_resource_id: None,
    }
}

fn symbol_candidate() -> SymbolCandidate {
    SymbolCandidate {
        symbol: Symbol {
            id: SymbolId::generate(),
            resource_id: ResourceId::generate(),
            parent_id: None,
            kind: SymbolKind::Function,
            name: "login".to_owned(),
            qualified_name: "login".to_owned(),
            signature: Some("fn login()".to_owned()),
            visibility: Visibility::Public,
            exported: true,
            span: span(),
            resource_revision: "rev-1".to_owned(),
            analysis_profile_id: 1,
        },
        path_rel: "src/auth.rs".to_owned(),
        coverage: StructuralCoverage::Complete,
    }
}

fn prepared_range(resource: ResourceId) -> PreparedRange {
    PreparedRange {
        resource,
        path_rel: "src/auth.rs".to_owned(),
        resource_revision: "rev-1".to_owned(),
        span: span(),
        source: "fn login() {}".to_owned(),
        role: RangeRole::EvidenceSpan,
        verification: SourceVerification {
            expected_content_hash: "h".to_owned(),
            observed_content_hash: "h".to_owned(),
            currentness: Currentness::Current,
        },
    }
}

fn related_test() -> EvidenceItem {
    let relation = relation();
    EvidenceItem::RelatedTest {
        target: relation.target.clone(),
        candidate: RelatedTestCandidate {
            resource: ResourceId::generate(),
            path_rel: "tests/auth.rs".to_owned(),
            endpoints: vec![relation.source.clone()],
            distance: 1,
            basis: ProjectionBasis::DirectRelation(RelationKind::Calls),
            paths: Vec::new(),
            paths_truncated: false,
            support: Support::Supported,
            freshness: Freshness::Fresh,
        },
    }
}

fn located(candidates: Vec<GraphEndpoint>, exact_selector: bool) -> Located<GraphEndpoint> {
    Located {
        candidates,
        last_valid: Vec::new(),
        exact_selector,
        truncated: false,
        currentness: Currentness::Current,
        source: ResultSource::StructuralIndex,
        incomplete_coverage: Vec::new(),
    }
}

fn coverage(report: CoverageReport, confirmed: usize) -> CoverageEvidence {
    CoverageEvidence {
        subject: CoverageSubject::Relations {
            anchor: GraphEndpoint::Symbol(SymbolId::generate()),
            direction: Direction::Incoming,
            kinds: vec![RelationKind::Calls],
        },
        report,
        confirmed,
    }
}

// ------------------------------------------------------ intent / target

#[test]
fn every_code_intent_requires_a_target() {
    for intent in [
        ProjectionIntent::Locate,
        ProjectionIntent::Understand,
        ProjectionIntent::Change(None),
        ProjectionIntent::Change(Some(ChangeKind::Delete)),
        ProjectionIntent::Impact(ChangeKind::Structural(ImpactIntent::Rename)),
    ] {
        let mut request = request(intent);
        assert!(request.validate().is_ok(), "{intent:?}");
        request.target = None;
        assert!(
            matches!(
                request.validate(),
                Err(ProjectionRequestError::MissingTarget)
            ),
            "{intent:?}"
        );
    }
}

#[test]
fn resume_handoff_requires_an_explicit_work_item_and_no_target() {
    let mut request =
        ProjectionRequest::new(WorkspaceId::generate(), ProjectionIntent::ResumeHandoff);
    assert!(matches!(
        request.validate(),
        Err(ProjectionRequestError::MissingWorkItem)
    ));
    request.work_item = Some(WorkItemId::generate());
    assert!(request.target.is_none());
    assert!(request.validate().is_ok());
    // A target is optional, not forbidden.
    request.target = Some(symbol_target(SymbolName::Name("login".to_owned())));
    assert!(request.validate().is_ok());
}

#[test]
fn change_profiles_reuse_impact_intent_and_add_only_missing_forms() {
    // Every I3 impact form is reachable through the existing enum.
    for intent in [
        ImpactIntent::PublicSignatureChange,
        ImpactIntent::Rename,
        ImpactIntent::ModuleMove,
        ImpactIntent::BaseInterfaceChange,
    ] {
        let kind = ChangeKind::Structural(intent);
        assert!(
            request(ProjectionIntent::Change(Some(kind)))
                .validate()
                .is_ok()
        );
        assert!(request(ProjectionIntent::Impact(kind)).validate().is_ok());
    }
    for kind in [ChangeKind::Delete, ChangeKind::DomainContractChange] {
        assert!(request(ProjectionIntent::Impact(kind)).validate().is_ok());
    }
}

#[test]
fn workspace_is_the_only_identity_and_base_of_applicability() {
    let request = request(ProjectionIntent::Locate);
    let context = request.validate().unwrap();
    // WorkspaceId is a required, non-optional field; no ProjectId exists
    // on the request. The Project layer is the keyless PROJECT scope.
    assert_eq!(context, ApplicabilityContext::base(Some(request.workspace)));
    assert_eq!(context.layers()[1], vec![KnowledgeScope::project()]);
}

#[test]
fn empty_selector_text_is_rejected() {
    for target in [
        ProjectionTarget::Resource(ResourceTarget::Path(String::new())),
        ProjectionTarget::Resource(ResourceTarget::Basename(String::new())),
        ProjectionTarget::Resource(ResourceTarget::PathPrefix(String::new())),
        symbol_target(SymbolName::QualifiedName(String::new())),
        symbol_target(SymbolName::Name(String::new())),
        symbol_target(SymbolName::PartialName(String::new())),
    ] {
        let mut request = request(ProjectionIntent::Understand);
        request.target = Some(target);
        assert!(matches!(
            request.validate(),
            Err(ProjectionRequestError::EmptySelector)
        ));
    }
}

#[test]
fn resource_selectors_convert_with_their_exact_or_search_meaning() {
    let id = ResourceId::generate();
    assert!(matches!(ResourceTarget::Id(id).locator(), ResourceLocator::Id(got) if got == id));
    let path = ResourceTarget::Path("src/a/app.ts".to_owned());
    assert!(matches!(
        path.locator(),
        ResourceLocator::Path("src/a/app.ts")
    ));
    // A basename stays a (possibly multi-candidate) search, never a path.
    let basename = ResourceTarget::Basename("app.ts".to_owned());
    assert!(matches!(
        basename.locator(),
        ResourceLocator::Basename("app.ts")
    ));
    let prefix = ResourceTarget::PathPrefix("src/".to_owned());
    assert!(matches!(
        prefix.locator(),
        ResourceLocator::PathPrefix("src/")
    ));
}

#[test]
fn symbol_selectors_convert_with_their_exact_or_search_meaning() {
    let id = SymbolId::generate();
    let query_of = |name| SymbolTarget::new(name);
    assert!(matches!(
        query_of(SymbolName::Id(id)).query().selector,
        SymbolSelector::Id(got) if got == id
    ));
    assert!(matches!(
        query_of(SymbolName::QualifiedName("Auth::login".to_owned()))
            .query()
            .selector,
        SymbolSelector::QualifiedName("Auth::login")
    ));
    // A name is not made unique, and a partial name stays a search.
    assert!(matches!(
        query_of(SymbolName::Name("run".to_owned()))
            .query()
            .selector,
        SymbolSelector::Name("run")
    ));
    assert!(matches!(
        query_of(SymbolName::PartialName("ru".to_owned()))
            .query()
            .selector,
        SymbolSelector::PartialName("ru")
    ));
}

#[test]
fn symbol_narrowing_maps_to_exact_resource_scope_kind_and_language() {
    let resource = ResourceId::generate();
    let target = SymbolTarget {
        name: SymbolName::Name("run".to_owned()),
        resource: Some(resource),
        kind: Some(SymbolKind::Method),
        language: Some(ResourceLanguage::Rust),
    };
    let query = target.query();
    assert!(matches!(query.scope, Some(ResourceScope::Id(got)) if got == resource));
    assert_eq!(query.kind, Some(SymbolKind::Method));
    assert_eq!(query.language, Some(ResourceLanguage::Rust));
    // No projection-side limit: the query's own default applies.
    assert_eq!(query.limit, None);
}

#[test]
fn exact_graph_endpoints_survive_unchanged() {
    for endpoint in [
        GraphEndpoint::Logical(LogicalSymbolId::generate()),
        GraphEndpoint::Domain(DomainEntity {
            kind: "ENV".to_owned(),
            normalized_identity: "DATABASE_URL".to_owned(),
            namespace: None,
            method: None,
            display_label: "DATABASE_URL".to_owned(),
        }),
    ] {
        let mut request = request(ProjectionIntent::Understand);
        request.target = Some(ProjectionTarget::Endpoint(endpoint.clone()));
        let copy = request.clone();
        assert!(request.validate().is_ok());
        assert_eq!(request, copy);
        assert_eq!(request.target, Some(ProjectionTarget::Endpoint(endpoint)));
    }
}

// ------------------------------------------------ applicability / directive

#[test]
fn exact_extra_scope_layer_is_appended_by_the_resolver_rules() {
    let mut request = request(ProjectionIntent::Change(None));
    let package = KnowledgeScope::keyed(ScopeKind::Package, "backend").unwrap();
    let module = KnowledgeScope::keyed(ScopeKind::Module, "auth").unwrap();
    request.scope_layers = vec![vec![package.clone(), module.clone()]];
    let context = request.validate().unwrap();
    let expected = ApplicabilityContext::base(Some(request.workspace))
        .with_layer(vec![package.clone(), module.clone()])
        .unwrap();
    assert_eq!(context, expected);
    assert_eq!(context.layer_of(&module), Some(3));

    // Order inside one layer carries no meaning.
    request.scope_layers = vec![vec![module, package]];
    assert_eq!(request.validate().unwrap().layers().len(), 4);
}

#[test]
fn duplicate_or_malformed_scope_layers_are_rejected() {
    let package = KnowledgeScope::keyed(ScopeKind::Package, "backend").unwrap();
    for layers in [
        vec![vec![package.clone(), package.clone()]],
        vec![vec![package.clone()], vec![package.clone()]],
        vec![Vec::new()],
        vec![vec![KnowledgeScope::project()]],
        vec![vec![KnowledgeScope::workspace(WorkspaceId::generate())]],
    ] {
        let mut request = request(ProjectionIntent::Locate);
        request.scope_layers = layers;
        assert!(matches!(
            request.validate(),
            Err(ProjectionRequestError::InvalidScopeLayer(
                ResolveError::InvalidContext(_)
            ))
        ));
    }
}

#[test]
fn directives_must_sit_inside_the_request_applicability() {
    let mut request = request(ProjectionIntent::Change(None));
    request.directives = vec![directive(
        "d1",
        KnowledgeScope::workspace(request.workspace),
    )];
    assert!(request.validate().is_ok());

    // An unlisted layer is not added to make the directive fit.
    let package = KnowledgeScope::keyed(ScopeKind::Package, "backend").unwrap();
    request.directives = vec![directive("d2", package.clone())];
    assert!(matches!(
        request.validate(),
        Err(ProjectionRequestError::DirectiveOutsideApplicability { directive_id }) if directive_id == "d2"
    ));
    request.scope_layers = vec![vec![package]];
    assert!(request.validate().is_ok());
}

#[test]
fn workspace_a_rejects_a_directive_for_workspace_b() {
    let mut request = request(ProjectionIntent::Locate);
    request.directives = vec![directive(
        "other",
        KnowledgeScope::workspace(WorkspaceId::generate()),
    )];
    assert!(matches!(
        request.validate(),
        Err(ProjectionRequestError::DirectiveOutsideApplicability { .. })
    ));
}

#[test]
fn directive_shape_is_checked_by_the_resolver_rules() {
    let mut request = request(ProjectionIntent::Locate);
    let scope = KnowledgeScope::project();
    request.directives = vec![directive("d", scope.clone()), directive("d", scope.clone())];
    assert!(matches!(
        request.validate(),
        Err(ProjectionRequestError::InvalidDirective(
            ResolveError::InvalidRequest(_)
        ))
    ));
    request.directives = vec![directive("", scope)];
    assert!(matches!(
        request.validate(),
        Err(ProjectionRequestError::InvalidDirective(_))
    ));
}

// -------------------------------------------------------------- correlation

#[test]
fn correlation_is_optional_and_opaque() {
    let bare = request(ProjectionIntent::Understand);
    assert!(bare.correlation.is_none());
    let base = bare.validate().unwrap();

    let mut hinted = bare.clone();
    hinted.correlation = Some(ProjectionCorrelation {
        client_id: Some("claude-code".to_owned()),
        session_id: Some("s-1".to_owned()),
        external_task_id: Some("T-9".to_owned()),
        external_subtask_id: None,
        role_hint: Some("backend".to_owned()),
        team_hint: Some("platform".to_owned()),
        persona_traits: BTreeSet::from(["security-reviewer".to_owned()]),
    });
    // Role/persona add no layer, no authority, and leave the target alone.
    assert_eq!(hinted.validate().unwrap(), base);
    assert_eq!(hinted.target, bare.target);
    assert_eq!(hinted.intent, bare.intent);
}

#[test]
fn role_hint_is_not_checked_against_the_target() {
    // "frontend" role on a backend path: a hint, not a permission rule.
    let mut request = request(ProjectionIntent::Change(None));
    request.target = Some(ProjectionTarget::Resource(ResourceTarget::Path(
        "backend/auth.rs".to_owned(),
    )));
    request.correlation = Some(ProjectionCorrelation {
        role_hint: Some("frontend".to_owned()),
        ..ProjectionCorrelation::default()
    });
    let with_frontend = request.validate().unwrap();
    request.correlation.as_mut().unwrap().role_hint = Some("backend".to_owned());
    assert_eq!(request.validate().unwrap(), with_frontend);
}

#[test]
fn empty_supplied_correlation_ids_are_rejected() {
    for (field, set) in [
        (
            CorrelationField::ClientId,
            (|c: &mut ProjectionCorrelation| c.client_id = Some(String::new()))
                as fn(&mut ProjectionCorrelation),
        ),
        (CorrelationField::SessionId, |c| {
            c.session_id = Some(String::new())
        }),
        (CorrelationField::ExternalTaskId, |c| {
            c.external_task_id = Some(String::new());
        }),
        (CorrelationField::ExternalSubtaskId, |c| {
            c.external_subtask_id = Some(String::new());
        }),
    ] {
        let mut correlation = ProjectionCorrelation::default();
        set(&mut correlation);
        let mut request = request(ProjectionIntent::Locate);
        request.correlation = Some(correlation);
        assert!(matches!(
            request.validate(),
            Err(ProjectionRequestError::EmptyCorrelationId(got)) if got == field
        ));
    }
}

#[test]
fn persona_trait_order_does_not_change_the_request() {
    let traits = |list: [&str; 2]| ProjectionCorrelation {
        persona_traits: list.iter().map(|&t| t.to_owned()).collect(),
        ..ProjectionCorrelation::default()
    };
    assert_eq!(traits(["terse", "risk"]), traits(["risk", "terse"]));
}

// ------------------------------------------------------------------ origin

#[test]
fn stored_facts_are_stored() {
    let decision = crate::knowledge::Decision {
        uid: DecisionId::generate(),
        scope: KnowledgeScope::project(),
        topic: "orm".to_owned(),
        chosen_summary: "none".to_owned(),
        rationale: "simple".to_owned(),
        status: crate::knowledge::DecisionStatus::Active,
        provenance: Provenance::new(SourceKind::UserExplicit),
        created_at: "t0".to_owned(),
        updated_at: "t0".to_owned(),
    };
    for item in [
        EvidenceItem::Resource(resource()),
        EvidenceItem::Symbol(symbol_candidate()),
        EvidenceItem::Relation(relation()),
        EvidenceItem::RelationGap(gap(Vec::new(), false)),
        EvidenceItem::Policy(resolved(policy(SourceKind::UserExplicit))),
        EvidenceItem::Decision(resolved(decision)),
        EvidenceItem::WorkingState(working_state()),
    ] {
        assert_eq!(item.origin(), EvidenceOrigin::Stored, "{item:?}");
    }
}

#[test]
fn observations_are_observed() {
    let range = EvidenceItem::CurrentSource(prepared_range(ResourceId::generate()));
    assert_eq!(range.origin(), EvidenceOrigin::Observed);
    let directive = EvidenceItem::Directive(Resolved {
        item: directive("d", KnowledgeScope::project()),
        origin: Origin::Request,
        layer: 1,
        reason: ResolutionReason::RequestExplicit,
    });
    assert_eq!(directive.origin(), EvidenceOrigin::Observed);
}

#[test]
fn deterministic_computations_are_derived() {
    let this = WorkItemId::generate();
    let conflict = KnowledgeConflict {
        kind: ConflictKind::ProtectedOverrideRejected,
        subject: Some("style".to_owned()),
        involved: vec![EvidenceRef {
            category: EvidenceCategory::Directive,
            id: "d".to_owned(),
            origin: Origin::Request,
            scope: KnowledgeScope::project(),
            layer: 1,
            source_kind: None,
            status: None,
        }],
    };
    for item in [
        related_test(),
        EvidenceItem::KnowledgeConflict(conflict),
        EvidenceItem::WorkOverlap {
            work_item: this,
            overlap: WorkOverlap {
                other: WorkItemId::generate(),
                other_status: WorkItemStatus::Active,
                resource: ResourceId::generate(),
                this_roles: vec![WorkResourceRole::Target],
                other_roles: vec![WorkResourceRole::Touched],
            },
        },
        EvidenceItem::Coverage(coverage(CoverageReport::new(), 0)),
        EvidenceItem::SourceUnavailable {
            resource: ResourceId::generate(),
            span: span(),
            reason: SourceUnavailable::NoCurrentSource {
                detail: "deleted".to_owned(),
            },
        },
        EvidenceItem::TargetSelection(TargetSelection {
            selector: symbol_target(SymbolName::Name("run".to_owned())),
            located: located(Vec::new(), true),
        }),
        EvidenceItem::IndexCurrentness {
            workspace: WorkspaceId::generate(),
            currentness: Currentness::NotCurrent(NotCurrentReason::ResourceIndexDirty),
        },
    ] {
        assert_eq!(item.origin(), EvidenceOrigin::Derived, "{item:?}");
    }
}

#[test]
fn origin_has_exactly_three_fact_only_values_and_no_override() {
    // Exhaustive without a wildcard: a fourth (estimated/inferred/...)
    // variant would fail to compile here.
    let name = |origin: EvidenceOrigin| match origin {
        EvidenceOrigin::Observed => "OBSERVED",
        EvidenceOrigin::Stored => "STORED",
        EvidenceOrigin::Derived => "DERIVED",
    };
    assert_eq!(name(EvidenceOrigin::Stored), "STORED");

    // Origin is a function of the variant: the same Policy payload is
    // STORED however it is built, and there is no origin field to set.
    let item = EvidenceItem::Policy(resolved(policy(SourceKind::Observed)));
    assert_eq!(item.origin(), EvidenceOrigin::Stored);
}

#[test]
fn policy_provenance_survives_beside_the_evidence_origin() {
    let policy = policy(SourceKind::UserExplicit);
    let item = EvidenceItem::Policy(resolved(policy.clone()));
    assert_eq!(item.origin(), EvidenceOrigin::Stored);
    let EvidenceItem::Policy(entry) = item else {
        unreachable!()
    };
    assert_eq!(entry.item.provenance, policy.provenance);
    assert_eq!(entry.item.provenance.source_kind, SourceKind::UserExplicit);
    assert_eq!(entry.item.uid, policy.uid);
    assert_eq!(entry.reason, ResolutionReason::SelectedPolicy);
}

// ---------------------------------------------------------- code evidence

#[test]
fn source_range_keeps_resource_revision_and_span() {
    let resource = ResourceId::generate();
    let EvidenceItem::CurrentSource(range) = EvidenceItem::CurrentSource(prepared_range(resource))
    else {
        unreachable!()
    };
    assert_eq!(range.resource, resource);
    assert_eq!(range.resource_revision, "rev-1");
    assert_eq!(range.span, span());
}

#[test]
fn relation_evidence_is_a_locator_that_matches_its_source_by_identity() {
    let relation = relation();
    let location = relation.evidence[0].clone();
    // RelationResult holds spans only; the body is a separate item that
    // matches on (resource, revision, span), not on a result-local RangeId.
    let range = prepared_range(location.resource);
    assert_eq!(
        (range.resource, range.resource_revision.as_str(), range.span),
        (
            location.resource,
            location.basis_revision.as_str(),
            location.span
        )
    );
    let items = [
        EvidenceItem::Relation(relation),
        EvidenceItem::CurrentSource(range),
    ];
    assert_eq!(items[0].origin(), EvidenceOrigin::Stored);
    assert_eq!(items[1].origin(), EvidenceOrigin::Observed);
}

#[test]
fn ambiguity_is_represented_without_choosing() {
    let first = GraphEndpoint::Symbol(SymbolId::generate());
    let second = GraphEndpoint::Symbol(SymbolId::generate());
    let mut selection = TargetSelection {
        selector: symbol_target(SymbolName::Name("run".to_owned())),
        located: located(vec![first.clone(), second], true),
    };
    selection.located.truncated = true;
    selection.located.currentness = Currentness::NotCurrent(NotCurrentReason::ResourceIndexDirty);
    assert!(selection.located.is_ambiguous());
    assert_eq!(selection.located.exact(), None);
    assert_eq!(selection.located.candidates.len(), 2);

    // Only an exact selector matching once is a single answer.
    let single = located(vec![first.clone()], true);
    assert_eq!(single.exact(), Some(&first));
    assert_eq!(located(vec![first], false).exact(), None);
}

#[test]
fn zero_results_keep_the_three_way_safe_negative() {
    let complete = coverage(CoverageReport::new(), 0);
    assert_eq!(
        complete.answer_state(),
        AnswerState::NoneUnderCompleteCoverage
    );
    assert!(complete.answer_state().is_safe_negative());

    let mut limits = CoverageReport::new();
    limits.note(CoverageLimit::UnresolvedEvidence);
    limits.note(CoverageLimit::TraversalTruncated);
    let incomplete = coverage(limits.clone(), 0);
    assert_eq!(
        incomplete.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );
    assert!(incomplete.report.has(CoverageLimit::TraversalTruncated));
    assert_eq!(coverage(limits, 2).answer_state(), AnswerState::Confirmed);
}

#[test]
fn source_unavailable_keeps_its_exact_reason() {
    let reason = SourceUnavailable::StaleBasis {
        basis_revision: "rev-1".to_owned(),
        current_revision: "rev-2".to_owned(),
    };
    let item = EvidenceItem::SourceUnavailable {
        resource: ResourceId::generate(),
        span: span(),
        reason: reason.clone(),
    };
    assert!(matches!(
        item,
        EvidenceItem::SourceUnavailable { reason: got, .. } if got == reason
    ));
}

#[test]
fn relation_gap_keeps_candidates_and_truncation() {
    let candidates = vec![
        GraphEndpoint::Resource(ResourceId::generate()),
        GraphEndpoint::Resource(ResourceId::generate()),
    ];
    let EvidenceItem::RelationGap(gap) = EvidenceItem::RelationGap(gap(candidates.clone(), true))
    else {
        unreachable!()
    };
    assert_eq!(gap.candidates, candidates);
    assert!(gap.candidate_truncated);
    assert_eq!(gap.resolution, Resolution::Candidate);
}

#[test]
fn work_overlap_and_generation_reference_name_their_work_item() {
    let this = WorkItemId::generate();
    let item = EvidenceItem::GenerationReference {
        work_item: this,
        basis: GenerationBasis::Baseline,
        reference: crate::knowledge::GenerationReference {
            index_incarnation: None,
            generation_no: 3,
            workspace_revision: "ws-1".to_owned(),
            state: crate::knowledge::GenerationReferenceState::HistoricalMissingOrReused,
        },
    };
    assert_eq!(item.origin(), EvidenceOrigin::Derived);
    assert!(
        matches!(item, EvidenceItem::GenerationReference { work_item, .. } if work_item == this)
    );
    let staleness = EvidenceItem::WorkStaleness {
        work_item: this,
        staleness: crate::knowledge::Staleness::PossiblyStale,
    };
    assert_eq!(staleness.origin(), EvidenceOrigin::Derived);
}

// --------------------------------------------------------------- boundary

/// Non-comment code lines of the contract module.
fn contract_code() -> String {
    include_str!("../projection.rs")
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn mixed_origin_composites_have_no_evidence_variant() {
    let code = contract_code();
    for composite in ["PreparedInspection", "ResolvedKnowledge", "WorkSnapshot"] {
        assert!(!code.contains(composite), "{composite}");
    }
}

#[test]
fn contract_has_no_confidence_capability_flags_budget_telemetry_or_ledger() {
    let code = contract_code().to_lowercase();
    for forbidden in [
        "confidence",
        "estimated",
        "inferred",
        "predicted",
        "recommended",
        "include_",
        "wake_backend",
        "max_items",
        "max_tokens",
        "budget",
        "continuation",
        "raw_available",
        "delivered",
        "ledger",
        "task_prompt",
        "natural_language",
        "projectid",
        "generation_no",
    ] {
        assert!(!code.contains(forbidden), "{forbidden}");
    }
}

#[test]
fn contract_has_no_sql_store_source_read_or_backend() {
    let code = contract_code();
    for forbidden in [
        "rusqlite",
        "Connection",
        "schema::",
        "QueryIndex",
        "SourceReader",
        "InspectPreparer",
        "RelationIndex",
        "ImpactTraversal",
        "RelatedTests::",
        "resolve(",
        "semantic",
        "std::fs",
    ] {
        assert!(!code.contains(forbidden), "{forbidden}");
    }
}

#[test]
fn schema_versions_are_unchanged() {
    assert_eq!(schema::global::GLOBAL_MIGRATIONS.len(), 4);
    assert_eq!(schema::project::PROJECT_MIGRATIONS.len(), 4);
    assert_eq!(schema::workspace::WORKSPACE_MIGRATIONS.len(), 5);
    assert_eq!(schema::index::INDEX_MIGRATIONS.len(), 10);
}
