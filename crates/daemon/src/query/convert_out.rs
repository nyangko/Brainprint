//! Task 10 Core result -> wire conversion (#24 §8, §9).
//!
//! Adapter formatting is not allowed to affect the Core result: every
//! function here is a lossless, order-preserving mirror. `core_error` is
//! the complete #24 §9 error mapping; raw SQLite/driver/debug text never
//! crosses it (unsafe detail is logged daemon-side only, via
//! [`log_internal`]).

use brainprint_core::protocol::query::*;
use brainprint_engine::{
    boundary::{
        BoundaryEdge, GapAggregate, GroupBasis, GroupCoverage, GroupSummary, MemberSample,
        StructuralGroup, StructuralSummary,
    },
    coverage::{AnswerState, CoverageLimit, CoverageReport},
    gaps::{IntendedRelation, UnresolvedReason},
    graph::{DomainEntity, ExternalEntity, GraphEndpoint},
    impact::ImpactIntent,
    inspect::SourceVerification,
    knowledge::{
        Blueprint, BlueprintApplication, BlueprintApplicationStatus, BlueprintComponent,
        BlueprintDefinition, BlueprintDefinitionState, BlueprintEvidence, BlueprintOwnerKind,
        BlueprintRef, BlueprintRelationship, BlueprintStatus, ConflictKind, Decision,
        DecisionLineage, DecisionLink, DecisionLinkKind, DecisionStatus, DirectiveTarget,
        EvidenceCategory, EvidenceRef, GenerationReference, GenerationReferenceState,
        KnowledgeConflict, KnowledgeScope, Origin, Policy, PolicyLineage, PolicyStatus,
        PreferenceStatus, PriorityClass, ProjectState, ProjectStateStatus, ProtectionClass,
        Provenance, RequestDirective, ResolutionReason, Resolved, ScopeKind, SourceKind, Staleness,
        TypedValue, UserPreference, WorkHandoff, WorkItem, WorkItemSourceKind, WorkItemStatus,
        WorkOverlap, WorkResourceRole, WorkResult, WorkResultStatus, WorkingState,
    },
    parser::{SourcePoint, SourceSpan},
    prepare::{PreparedRange, RangeRole, SourceUnavailable},
    projection::{
        ChangeKind, CoverageEvidence, CoverageSubject, EvidenceItem, GenerationBasis,
        TargetSelection,
        planner::{
            ContinuationUnavailable, DeliveryDimension, DeliveryPage, Measure, PendingDelivery,
            ProjectionEconomy, ProjectionGap, ReuseIdentity, ReuseReference,
        },
    },
    query::{Currentness, FileListing, NotCurrentReason, ResultSource, StructuralCoverage},
    query_surface::{
        CoreError, FindResult, InvalidRequest, KnowledgeResult, NotInitialized, ProjectedAnswer,
        RelationsResult, TargetResolution,
    },
    related_tests::{ProjectionBasis, RelatedTestCandidate, TestPath},
    relations::{
        Coverage, Direction, EvidenceLocation, GapAttribution, RelationAnswer, RelationGap,
        RelationResult, ScopeState,
    },
    resolution::{Dispatch, Freshness, Resolution, Support, TargetScope},
    resource::{Resource, ResourceState},
    search::{
        BudgetAxis, FallbackReason, MatchSource, QueryStatus, ScopeReport, TextMatch,
        TextSearchResult,
    },
    symbol::{OccurrenceKind, Symbol, SymbolKind, Visibility},
};

fn hex32(bytes: [u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// ------------------------------------------------------------ target

fn symbol_kind_wire(kind: SymbolKind) -> SymbolKindWire {
    match kind {
        SymbolKind::Class => SymbolKindWire::Class,
        SymbolKind::Interface => SymbolKindWire::Interface,
        SymbolKind::Struct => SymbolKindWire::Struct,
        SymbolKind::Enum => SymbolKindWire::Enum,
        SymbolKind::Trait => SymbolKindWire::Trait,
        SymbolKind::Record => SymbolKindWire::Record,
        SymbolKind::Function => SymbolKindWire::Function,
        SymbolKind::Method => SymbolKindWire::Method,
        SymbolKind::TypeAlias => SymbolKindWire::TypeAlias,
        SymbolKind::Field => SymbolKindWire::Field,
        SymbolKind::Property => SymbolKindWire::Property,
        SymbolKind::Constant => SymbolKindWire::Constant,
    }
}

fn resource_language_wire(
    language: brainprint_engine::resource::ResourceLanguage,
) -> ResourceLanguageWire {
    use brainprint_engine::resource::ResourceLanguage as L;
    match language {
        L::Python => ResourceLanguageWire::Python,
        L::JavaScript => ResourceLanguageWire::JavaScript,
        L::TypeScript => ResourceLanguageWire::TypeScript,
        L::Svelte => ResourceLanguageWire::Svelte,
        L::CSharp => ResourceLanguageWire::CSharp,
        L::Rust => ResourceLanguageWire::Rust,
    }
}

fn resource_role_wire(role: brainprint_engine::resource::ResourceRole) -> ResourceRoleWire {
    use brainprint_engine::resource::ResourceRole as R;
    match role {
        R::Source => ResourceRoleWire::Source,
        R::Test => ResourceRoleWire::Test,
        R::Config => ResourceRoleWire::Config,
        R::Docs => ResourceRoleWire::Docs,
        R::Generated => ResourceRoleWire::Generated,
        R::Dependency => ResourceRoleWire::Dependency,
        R::Asset => ResourceRoleWire::Asset,
        R::ToolState => ResourceRoleWire::ToolState,
        R::Unknown => ResourceRoleWire::Unknown,
    }
}

fn resource_kind_wire(kind: brainprint_engine::resource::ResourceKind) -> ResourceKindWire {
    use brainprint_engine::resource::ResourceKind as K;
    match kind {
        K::File => ResourceKindWire::File,
        K::Directory => ResourceKindWire::Directory,
    }
}

fn relation_kind_wire(kind: brainprint_engine::graph::RelationKind) -> RelationKindWire {
    use brainprint_engine::graph::RelationKind as K;
    match kind {
        K::Calls => RelationKindWire::Calls,
        K::References => RelationKindWire::References,
        K::Imports => RelationKindWire::Imports,
        K::Extends => RelationKindWire::Extends,
        K::Implements => RelationKindWire::Implements,
        K::Overrides => RelationKindWire::Overrides,
        K::UsesType => RelationKindWire::UsesType,
        K::UsesEnv => RelationKindWire::UsesEnv,
        K::UsesConfig => RelationKindWire::UsesConfig,
    }
}

fn graph_endpoint_wire(endpoint: GraphEndpoint) -> GraphEndpointWire {
    match endpoint {
        GraphEndpoint::Resource(id) => GraphEndpointWire::Resource(id),
        GraphEndpoint::Symbol(id) => GraphEndpointWire::Symbol(id),
        GraphEndpoint::External(entity) => {
            GraphEndpointWire::External(external_entity_wire(entity))
        }
        GraphEndpoint::Domain(entity) => GraphEndpointWire::Domain(domain_entity_wire(entity)),
        GraphEndpoint::Logical(id) => GraphEndpointWire::Logical(id),
    }
}

fn external_entity_wire(entity: ExternalEntity) -> ExternalEntityWire {
    ExternalEntityWire {
        package_identity: entity.package_identity,
        module_path: entity.module_path,
        symbol_name: entity.symbol_name,
        qualified_name: entity.qualified_name,
        kind: entity.kind,
        resolved_version: entity.resolved_version,
        declaration_locator: entity.declaration_locator,
    }
}

fn domain_entity_wire(entity: DomainEntity) -> DomainEntityWire {
    DomainEntityWire {
        kind: entity.kind,
        normalized_identity: entity.normalized_identity,
        namespace: entity.namespace,
        method: entity.method,
        display_label: entity.display_label,
    }
}

fn projection_target_wire(
    target: brainprint_engine::projection::ProjectionTarget,
) -> ProjectionTargetWire {
    use brainprint_engine::projection::{ProjectionTarget as T, ResourceTarget, SymbolName};
    match target {
        T::Endpoint(endpoint) => ProjectionTargetWire::Endpoint(graph_endpoint_wire(endpoint)),
        T::Resource(resource) => ProjectionTargetWire::Resource(match resource {
            ResourceTarget::Id(id) => ResourceTargetWire::Id(id),
            ResourceTarget::Path(path) => ResourceTargetWire::Path(path),
            ResourceTarget::Basename(name) => ResourceTargetWire::Basename(name),
            ResourceTarget::PathPrefix(prefix) => ResourceTargetWire::PathPrefix(prefix),
        }),
        T::Symbol(symbol) => ProjectionTargetWire::Symbol(SymbolTargetWire {
            name: match symbol.name {
                SymbolName::Id(id) => SymbolNameWire::Id(id),
                SymbolName::QualifiedName(name) => SymbolNameWire::QualifiedName(name),
                SymbolName::Name(name) => SymbolNameWire::Name(name),
                SymbolName::PartialName(name) => SymbolNameWire::PartialName(name),
            },
            resource: symbol.resource,
            kind: symbol.kind.map(symbol_kind_wire),
            language: symbol.language.map(resource_language_wire),
        }),
    }
}

// ----------------------------------------------------------- common

fn source_point_wire(point: SourcePoint) -> SourcePointWire {
    SourcePointWire {
        line: point.line,
        column: point.column,
    }
}

fn source_span_wire(span: SourceSpan) -> SourceSpanWire {
    SourceSpanWire {
        start_byte: span.start_byte,
        end_byte: span.end_byte,
        start: source_point_wire(span.start),
        end: source_point_wire(span.end),
    }
}

fn not_current_reason_wire(reason: NotCurrentReason) -> NotCurrentReasonWire {
    match reason {
        NotCurrentReason::ResourceIndexDirty => NotCurrentReasonWire::ResourceIndexDirty,
        NotCurrentReason::ResourceIndexNeverPublished => {
            NotCurrentReasonWire::ResourceIndexNeverPublished
        }
    }
}

fn currentness_wire(currentness: Currentness) -> CurrentnessWire {
    match currentness {
        Currentness::Current => CurrentnessWire::Current,
        Currentness::NotCurrent(reason) => {
            CurrentnessWire::NotCurrent(not_current_reason_wire(reason))
        }
    }
}

fn result_source_wire(source: ResultSource) -> ResultSourceWire {
    match source {
        ResultSource::StructuralIndex => ResultSourceWire::StructuralIndex,
    }
}

fn structural_coverage_wire(coverage: StructuralCoverage) -> StructuralCoverageWire {
    match coverage {
        StructuralCoverage::Complete => StructuralCoverageWire::Complete,
        StructuralCoverage::Partial => StructuralCoverageWire::Partial,
        StructuralCoverage::ContainerOnly => StructuralCoverageWire::ContainerOnly,
        StructuralCoverage::Unsupported => StructuralCoverageWire::Unsupported,
        StructuralCoverage::GeneratedUnmapped => StructuralCoverageWire::GeneratedUnmapped,
    }
}

fn coverage_note_wire(note: brainprint_engine::query::CoverageNote) -> CoverageNoteWire {
    CoverageNoteWire {
        resource_id: note.resource_id,
        path_rel: note.path_rel,
        coverage: structural_coverage_wire(note.coverage),
    }
}

fn resource_state_wire(state: ResourceState) -> ResourceStateWire {
    match state {
        ResourceState::Active => ResourceStateWire::Active,
        ResourceState::Deleted => ResourceStateWire::Deleted,
    }
}

fn resource_wire(resource: Resource) -> ResourceWire {
    ResourceWire {
        id: resource.id,
        path_rel: resource.path_rel,
        path_key: resource.path_key,
        kind: resource_kind_wire(resource.kind),
        role: resource_role_wire(resource.role),
        language: resource.language.map(resource_language_wire),
        size_bytes: resource.size_bytes,
        mtime_ns: resource.mtime_ns,
        fingerprint: resource.fingerprint,
        content_hash: resource.content_hash,
        state: resource_state_wire(resource.state),
        resource_revision: resource.resource_revision,
        generated_kind: resource.generated_kind,
        container_resource_id: resource.container_resource_id,
    }
}

fn visibility_wire(visibility: Visibility) -> VisibilityWire {
    match visibility {
        Visibility::Public => VisibilityWire::Public,
        Visibility::Private => VisibilityWire::Private,
        Visibility::Protected => VisibilityWire::Protected,
        Visibility::Internal => VisibilityWire::Internal,
        Visibility::Unspecified => VisibilityWire::Unspecified,
    }
}

fn symbol_wire(symbol: Symbol) -> SymbolWire {
    SymbolWire {
        id: symbol.id,
        resource_id: symbol.resource_id,
        parent_id: symbol.parent_id,
        kind: symbol_kind_wire(symbol.kind),
        name: symbol.name,
        qualified_name: symbol.qualified_name,
        signature: symbol.signature,
        visibility: visibility_wire(symbol.visibility),
        exported: symbol.exported,
        span: source_span_wire(symbol.span),
        resource_revision: symbol.resource_revision,
        analysis_profile_id: symbol.analysis_profile_id,
    }
}

fn symbol_candidate_wire(
    candidate: brainprint_engine::query::SymbolCandidate,
) -> SymbolCandidateWire {
    SymbolCandidateWire {
        symbol: symbol_wire(candidate.symbol),
        path_rel: candidate.path_rel,
        coverage: structural_coverage_wire(candidate.coverage),
    }
}

fn support_wire(support: Support) -> SupportWire {
    match support {
        Support::Supported => SupportWire::Supported,
        Support::Partial => SupportWire::Partial,
        Support::Unsupported => SupportWire::Unsupported,
    }
}

fn freshness_wire(freshness: Freshness) -> FreshnessWire {
    match freshness {
        Freshness::Fresh => FreshnessWire::Fresh,
        Freshness::Dirty => FreshnessWire::Dirty,
        Freshness::Stale => FreshnessWire::Stale,
    }
}

fn dispatch_wire(dispatch: Dispatch) -> DispatchWire {
    match dispatch {
        Dispatch::Static => DispatchWire::Static,
        Dispatch::Dynamic => DispatchWire::Dynamic,
        Dispatch::Unknown => DispatchWire::Unknown,
    }
}

fn target_scope_wire(scope: TargetScope) -> TargetScopeWire {
    match scope {
        TargetScope::Internal => TargetScopeWire::Internal,
        TargetScope::External => TargetScopeWire::External,
    }
}

fn occurrence_kind_wire(kind: OccurrenceKind) -> OccurrenceKindWire {
    match kind {
        OccurrenceKind::Definition => OccurrenceKindWire::Definition,
        OccurrenceKind::ImportSite => OccurrenceKindWire::ImportSite,
        OccurrenceKind::CallSite => OccurrenceKindWire::CallSite,
        OccurrenceKind::ReferenceSite => OccurrenceKindWire::ReferenceSite,
        OccurrenceKind::TypeSite => OccurrenceKindWire::TypeSite,
        OccurrenceKind::KeySite => OccurrenceKindWire::KeySite,
    }
}

fn evidence_location_wire(location: EvidenceLocation) -> EvidenceLocationWire {
    EvidenceLocationWire {
        resource: location.resource,
        containing_symbol: location.containing_symbol,
        occurrence_kind: occurrence_kind_wire(location.occurrence_kind),
        span: source_span_wire(location.span),
        basis_revision: location.basis_revision,
        support: support_wire(location.support),
        freshness: freshness_wire(location.freshness),
    }
}

// -------------------------------------------------------------- relations

fn resolution_wire(resolution: Resolution) -> ResolutionWire {
    match resolution {
        Resolution::Resolved => ResolutionWire::Resolved,
        Resolution::Candidate => ResolutionWire::Candidate,
        Resolution::Unresolved => ResolutionWire::Unresolved,
    }
}

fn direction_wire(direction: Direction) -> DirectionWire {
    match direction {
        Direction::Outgoing => DirectionWire::Outgoing,
        Direction::Incoming => DirectionWire::Incoming,
    }
}

fn relation_result_wire(result: RelationResult) -> RelationResultWire {
    RelationResultWire {
        kind: relation_kind_wire(result.kind),
        source: graph_endpoint_wire(result.source),
        target: graph_endpoint_wire(result.target),
        direction: direction_wire(result.direction),
        dispatch: dispatch_wire(result.dispatch),
        target_scope: target_scope_wire(result.target_scope),
        resolution: resolution_wire(result.resolution),
        support: support_wire(result.support),
        freshness: freshness_wire(result.freshness),
        evidence: result
            .evidence
            .into_iter()
            .map(evidence_location_wire)
            .collect(),
    }
}

fn intended_relation_wire(intended: IntendedRelation) -> IntendedRelationWire {
    match intended {
        IntendedRelation::Known(kind) => IntendedRelationWire::Known(relation_kind_wire(kind)),
        IntendedRelation::Inheritance => IntendedRelationWire::Inheritance,
    }
}

fn unresolved_reason_wire(reason: UnresolvedReason) -> UnresolvedReasonWire {
    match reason {
        UnresolvedReason::MissingRelativeTarget => UnresolvedReasonWire::MissingRelativeTarget,
        UnresolvedReason::ModuleTreeRequiresSemantics => {
            UnresolvedReasonWire::ModuleTreeRequiresSemantics
        }
        UnresolvedReason::NamespaceRequiresSemantics => {
            UnresolvedReasonWire::NamespaceRequiresSemantics
        }
        UnresolvedReason::ConfigDependentSpecifier => {
            UnresolvedReasonWire::ConfigDependentSpecifier
        }
        UnresolvedReason::CompoundSpecifier => UnresolvedReasonWire::CompoundSpecifier,
        UnresolvedReason::AmbiguousCandidates => UnresolvedReasonWire::AmbiguousCandidates,
        UnresolvedReason::PossiblyShadowed => UnresolvedReasonWire::PossiblyShadowed,
        UnresolvedReason::ReceiverTypeRequired => UnresolvedReasonWire::ReceiverTypeRequired,
        UnresolvedReason::NoStructuralBinding => UnresolvedReasonWire::NoStructuralBinding,
        UnresolvedReason::ImportTargetUnresolved => UnresolvedReasonWire::ImportTargetUnresolved,
        UnresolvedReason::NameNotInModule => UnresolvedReasonWire::NameNotInModule,
        UnresolvedReason::NotANameExpression => UnresolvedReasonWire::NotANameExpression,
        UnresolvedReason::TypeSemanticsRequired => UnresolvedReasonWire::TypeSemanticsRequired,
        UnresolvedReason::RelationKindNotStructural => {
            UnresolvedReasonWire::RelationKindNotStructural
        }
        UnresolvedReason::OverrideTargetRequiresSemantics => {
            UnresolvedReasonWire::OverrideTargetRequiresSemantics
        }
        UnresolvedReason::DynamicKeyExpression => UnresolvedReasonWire::DynamicKeyExpression,
    }
}

fn relation_gap_wire(gap: RelationGap) -> RelationGapWire {
    RelationGapWire {
        location: evidence_location_wire(gap.location),
        intended: intended_relation_wire(gap.intended),
        lookup_name: gap.lookup_name,
        module_hint: gap.module_hint,
        reason: unresolved_reason_wire(gap.reason),
        resolution: resolution_wire(gap.resolution),
        candidates: gap
            .candidates
            .into_iter()
            .map(graph_endpoint_wire)
            .collect(),
        candidate_truncated: gap.candidate_truncated,
        resolution_context_key: gap.resolution_context_key,
    }
}

fn gap_attribution_wire(attribution: GapAttribution) -> GapAttributionWire {
    match attribution {
        GapAttribution::SourceScoped => GapAttributionWire::SourceScoped,
        GapAttribution::TargetCandidateOnly => GapAttributionWire::TargetCandidateOnly,
    }
}

fn semantic_scope_wire(scope: brainprint_engine::merge::SemanticScope) -> SemanticScopeWire {
    SemanticScopeWire {
        contexts: scope.contexts,
        conflicts: scope.conflicts,
        not_current: scope.not_current,
    }
}

fn scope_state_wire(state: ScopeState) -> ScopeStateWire {
    ScopeStateWire {
        resource: state.resource,
        support: support_wire(state.support),
        freshness: freshness_wire(state.freshness),
    }
}

fn coverage_wire(coverage: Coverage) -> CoverageWire {
    CoverageWire {
        attribution: gap_attribution_wire(coverage.attribution),
        gaps: coverage.gaps,
        ambiguous: coverage.ambiguous,
        requires_semantics: coverage.requires_semantics,
        unsupported_construct: coverage.unsupported_construct,
        truncated: coverage.truncated,
        unattributed: coverage.unattributed,
        scope: coverage.scope.map(scope_state_wire),
        semantic: semantic_scope_wire(coverage.semantic),
    }
}

fn relation_answer_wire(answer: RelationAnswer) -> RelationAnswerWire {
    RelationAnswerWire {
        direction: direction_wire(answer.direction),
        kinds: answer.kinds.into_iter().map(relation_kind_wire).collect(),
        confirmed: answer
            .confirmed
            .into_iter()
            .map(relation_result_wire)
            .collect(),
        gaps: answer.gaps.into_iter().map(relation_gap_wire).collect(),
        coverage: coverage_wire(answer.coverage),
    }
}

fn projection_basis_wire(basis: ProjectionBasis) -> ProjectionBasisWire {
    match basis {
        ProjectionBasis::DirectRelation(kind) => {
            ProjectionBasisWire::DirectRelation(relation_kind_wire(kind))
        }
        ProjectionBasis::RelationPath { hops, first } => ProjectionBasisWire::RelationPath {
            hops,
            first: relation_kind_wire(first),
        },
    }
}

fn test_path_wire(path: TestPath) -> TestPathWire {
    TestPathWire {
        hops: path.hops.into_iter().map(relation_result_wire).collect(),
    }
}

fn related_test_candidate_wire(candidate: RelatedTestCandidate) -> RelatedTestCandidateWire {
    RelatedTestCandidateWire {
        resource: candidate.resource,
        path_rel: candidate.path_rel,
        endpoints: candidate
            .endpoints
            .into_iter()
            .map(graph_endpoint_wire)
            .collect(),
        distance: candidate.distance,
        basis: projection_basis_wire(candidate.basis),
        paths: candidate.paths.into_iter().map(test_path_wire).collect(),
        paths_truncated: candidate.paths_truncated,
        support: support_wire(candidate.support),
        freshness: freshness_wire(candidate.freshness),
    }
}

// ------------------------------------------------------------- knowledge

fn scope_kind_wire(kind: ScopeKind) -> ScopeKindWire {
    match kind {
        ScopeKind::Global => ScopeKindWire::Global,
        ScopeKind::Project => ScopeKindWire::Project,
        ScopeKind::Workspace => ScopeKindWire::Workspace,
        ScopeKind::Package => ScopeKindWire::Package,
        ScopeKind::Module => ScopeKindWire::Module,
        ScopeKind::Directory => ScopeKindWire::Directory,
        ScopeKind::Resource => ScopeKindWire::Resource,
        ScopeKind::Domain => ScopeKindWire::Domain,
        ScopeKind::Task => ScopeKindWire::Task,
    }
}

fn knowledge_scope_wire(scope: KnowledgeScope) -> KnowledgeScopeWire {
    KnowledgeScopeWire {
        kind: scope_kind_wire(scope.kind()),
        key: scope.key().map(str::to_owned),
    }
}

fn source_kind_wire(kind: SourceKind) -> SourceKindWire {
    match kind {
        SourceKind::UserExplicit => SourceKindWire::UserExplicit,
        SourceKind::AuthoritativeArtifact => SourceKindWire::AuthoritativeArtifact,
        SourceKind::Observed => SourceKindWire::Observed,
        SourceKind::AgentReported => SourceKindWire::AgentReported,
    }
}

fn provenance_wire(provenance: Provenance) -> ProvenanceWire {
    ProvenanceWire {
        source_kind: source_kind_wire(provenance.source_kind),
        locator: provenance.locator,
        revision: provenance.revision,
    }
}

fn typed_value_wire(value: TypedValue) -> TypedValueWire {
    match value {
        TypedValue::Text(text) => TypedValueWire::Text(text),
        TypedValue::Integer(value) => TypedValueWire::Integer(value),
        TypedValue::Boolean(value) => TypedValueWire::Boolean(value),
        TypedValue::Json(value) => TypedValueWire::Json(value),
    }
}

fn directive_target_wire(target: DirectiveTarget) -> DirectiveTargetWire {
    match target {
        DirectiveTarget::Policy => DirectiveTargetWire::Policy,
        DirectiveTarget::Decision => DirectiveTargetWire::Decision,
        DirectiveTarget::Preference => DirectiveTargetWire::Preference,
    }
}

fn request_directive_wire(directive: RequestDirective) -> RequestDirectiveWire {
    RequestDirectiveWire {
        id: directive.id,
        target: directive_target_wire(directive.target),
        subject_key: directive.subject_key,
        scope: knowledge_scope_wire(directive.scope),
        summary: directive.summary,
    }
}

fn protection_class_wire(class: ProtectionClass) -> ProtectionClassWire {
    match class {
        ProtectionClass::Normal => ProtectionClassWire::Normal,
        ProtectionClass::ProtectedPrivacy => ProtectionClassWire::ProtectedPrivacy,
        ProtectionClass::ProtectedSecurity => ProtectionClassWire::ProtectedSecurity,
    }
}

fn priority_class_wire(class: PriorityClass) -> PriorityClassWire {
    match class {
        PriorityClass::Default => PriorityClassWire::Default,
    }
}

fn policy_status_wire(status: PolicyStatus) -> PolicyStatusWire {
    match status {
        PolicyStatus::Active => PolicyStatusWire::Active,
        PolicyStatus::Superseded => PolicyStatusWire::Superseded,
        PolicyStatus::Disabled => PolicyStatusWire::Disabled,
    }
}

fn policy_wire(policy: Policy) -> PolicyWire {
    PolicyWire {
        uid: policy.uid,
        scope: knowledge_scope_wire(policy.scope),
        policy_key: policy.policy_key,
        title: policy.title,
        rule_text: policy.rule_text,
        structured_rule: policy.structured_rule,
        protection_class: protection_class_wire(policy.protection_class),
        priority_class: priority_class_wire(policy.priority_class),
        status: policy_status_wire(policy.status),
        provenance: provenance_wire(policy.provenance),
        created_at: policy.created_at,
        updated_at: policy.updated_at,
    }
}

fn policy_lineage_wire(lineage: PolicyLineage) -> PolicyLineageWire {
    PolicyLineageWire {
        supersedes: lineage.supersedes,
        superseded_by: lineage.superseded_by,
    }
}

fn decision_status_wire(status: DecisionStatus) -> DecisionStatusWire {
    match status {
        DecisionStatus::Active => DecisionStatusWire::Active,
        DecisionStatus::Superseded => DecisionStatusWire::Superseded,
        DecisionStatus::Reversed => DecisionStatusWire::Reversed,
    }
}

fn decision_wire(decision: Decision) -> DecisionWire {
    DecisionWire {
        uid: decision.uid,
        scope: knowledge_scope_wire(decision.scope),
        topic: decision.topic,
        chosen_summary: decision.chosen_summary,
        rationale: decision.rationale,
        status: decision_status_wire(decision.status),
        provenance: provenance_wire(decision.provenance),
        created_at: decision.created_at,
        updated_at: decision.updated_at,
    }
}

fn decision_link_kind_wire(kind: DecisionLinkKind) -> DecisionLinkKindWire {
    match kind {
        DecisionLinkKind::Supersedes => DecisionLinkKindWire::Supersedes,
        DecisionLinkKind::Reverses => DecisionLinkKindWire::Reverses,
    }
}

fn decision_link_wire(link: DecisionLink) -> DecisionLinkWire {
    DecisionLinkWire {
        kind: decision_link_kind_wire(link.kind),
        other: link.other,
    }
}

fn decision_lineage_wire(lineage: DecisionLineage) -> DecisionLineageWire {
    DecisionLineageWire {
        outgoing: lineage
            .outgoing
            .into_iter()
            .map(decision_link_wire)
            .collect(),
        incoming: lineage
            .incoming
            .into_iter()
            .map(decision_link_wire)
            .collect(),
    }
}

fn preference_status_wire(status: PreferenceStatus) -> PreferenceStatusWire {
    match status {
        PreferenceStatus::Active => PreferenceStatusWire::Active,
        PreferenceStatus::Superseded => PreferenceStatusWire::Superseded,
        PreferenceStatus::Disabled => PreferenceStatusWire::Disabled,
    }
}

fn user_preference_wire(preference: UserPreference) -> UserPreferenceWire {
    UserPreferenceWire {
        uid: preference.uid,
        scope: knowledge_scope_wire(preference.scope),
        preference_key: preference.preference_key,
        value: typed_value_wire(preference.value),
        status: preference_status_wire(preference.status),
        provenance: provenance_wire(preference.provenance),
        created_at: preference.created_at,
        updated_at: preference.updated_at,
    }
}

fn project_state_status_wire(status: ProjectStateStatus) -> ProjectStateStatusWire {
    match status {
        ProjectStateStatus::Current => ProjectStateStatusWire::Current,
        ProjectStateStatus::Retired => ProjectStateStatusWire::Retired,
    }
}

fn project_state_wire(state: ProjectState) -> ProjectStateWire {
    ProjectStateWire {
        uid: state.uid,
        key: state.key,
        scope: knowledge_scope_wire(state.scope),
        value: typed_value_wire(state.value),
        status: project_state_status_wire(state.status),
        provenance: provenance_wire(state.provenance),
        updated_at: state.updated_at,
    }
}

fn blueprint_status_wire(status: BlueprintStatus) -> BlueprintStatusWire {
    match status {
        BlueprintStatus::Draft => BlueprintStatusWire::Draft,
        BlueprintStatus::Active => BlueprintStatusWire::Active,
        BlueprintStatus::Retired => BlueprintStatusWire::Retired,
    }
}

fn blueprint_owner_kind_wire(owner: BlueprintOwnerKind) -> BlueprintOwnerKindWire {
    match owner {
        BlueprintOwnerKind::Global => BlueprintOwnerKindWire::Global,
        BlueprintOwnerKind::Project => BlueprintOwnerKindWire::Project,
    }
}

fn blueprint_ref_wire(reference: BlueprintRef) -> BlueprintRefWire {
    BlueprintRefWire {
        owner: blueprint_owner_kind_wire(reference.owner),
        uid: reference.uid,
    }
}

fn blueprint_component_wire(component: BlueprintComponent) -> BlueprintComponentWire {
    BlueprintComponentWire {
        name: component.name,
        description: component.description,
    }
}

fn blueprint_relationship_wire(relationship: BlueprintRelationship) -> BlueprintRelationshipWire {
    BlueprintRelationshipWire {
        from: relationship.from,
        to: relationship.to,
        kind: relationship.kind,
        description: relationship.description,
    }
}

fn blueprint_definition_wire(definition: BlueprintDefinition) -> BlueprintDefinitionWire {
    BlueprintDefinitionWire {
        components: definition
            .components
            .into_iter()
            .map(blueprint_component_wire)
            .collect(),
        relationships: definition
            .relationships
            .into_iter()
            .map(blueprint_relationship_wire)
            .collect(),
        constraints: definition.constraints,
    }
}

fn blueprint_wire(blueprint: Blueprint) -> BlueprintWire {
    BlueprintWire {
        uid: blueprint.uid,
        scope: knowledge_scope_wire(blueprint.scope),
        blueprint_key: blueprint.blueprint_key,
        title: blueprint.title,
        intent: blueprint.intent,
        definition: blueprint_definition_wire(blueprint.definition),
        status: blueprint_status_wire(blueprint.status),
        version: blueprint.version,
        provenance: provenance_wire(blueprint.provenance),
        created_at: blueprint.created_at,
        updated_at: blueprint.updated_at,
    }
}

fn blueprint_definition_state_wire(
    state: BlueprintDefinitionState,
) -> BlueprintDefinitionStateWire {
    match state {
        BlueprintDefinitionState::Available(blueprint) => {
            BlueprintDefinitionStateWire::Available(Box::new(blueprint_wire(*blueprint)))
        }
        BlueprintDefinitionState::Missing => BlueprintDefinitionStateWire::Missing,
        BlueprintDefinitionState::Retired => BlueprintDefinitionStateWire::Retired,
    }
}

fn blueprint_application_status_wire(
    status: BlueprintApplicationStatus,
) -> BlueprintApplicationStatusWire {
    match status {
        BlueprintApplicationStatus::Active => BlueprintApplicationStatusWire::Active,
        BlueprintApplicationStatus::Retired => BlueprintApplicationStatusWire::Retired,
    }
}

fn blueprint_application_wire(application: BlueprintApplication) -> BlueprintApplicationWire {
    BlueprintApplicationWire {
        uid: application.uid,
        blueprint: blueprint_ref_wire(application.blueprint),
        scope: knowledge_scope_wire(application.scope),
        status: blueprint_application_status_wire(application.status),
        application_summary: application.application_summary,
        provenance: provenance_wire(application.provenance),
        created_at: application.created_at,
        updated_at: application.updated_at,
    }
}

fn blueprint_evidence_wire(evidence: BlueprintEvidence) -> BlueprintEvidenceWire {
    BlueprintEvidenceWire {
        application: blueprint_application_wire(evidence.application),
        definition: blueprint_definition_state_wire(evidence.definition),
    }
}

fn origin_wire(origin: Origin) -> OriginWire {
    match origin {
        Origin::Request => OriginWire::Request,
        Origin::Global => OriginWire::Global,
        Origin::Project => OriginWire::Project,
        Origin::Workspace => OriginWire::Workspace,
    }
}

fn resolution_reason_wire(reason: ResolutionReason) -> ResolutionReasonWire {
    match reason {
        ResolutionReason::ProtectedConstraint => ResolutionReasonWire::ProtectedConstraint,
        ResolutionReason::RequestExplicit => ResolutionReasonWire::RequestExplicit,
        ResolutionReason::UnkeyedPolicy => ResolutionReasonWire::UnkeyedPolicy,
        ResolutionReason::SelectedPolicy => ResolutionReasonWire::SelectedPolicy,
        ResolutionReason::ResolvedDecision => ResolutionReasonWire::ResolvedDecision,
        ResolutionReason::AppliedPreference => ResolutionReasonWire::AppliedPreference,
        ResolutionReason::BlueprintEvidence => ResolutionReasonWire::BlueprintEvidence,
        ResolutionReason::StateEvidence => ResolutionReasonWire::StateEvidence,
        ResolutionReason::ShadowedByRequest => ResolutionReasonWire::ShadowedByRequest,
        ResolutionReason::ShadowedByProjectTier => ResolutionReasonWire::ShadowedByProjectTier,
        ResolutionReason::ShadowedByMoreSpecificScope => {
            ResolutionReasonWire::ShadowedByMoreSpecificScope
        }
        ResolutionReason::ShadowedByProjectDecision => {
            ResolutionReasonWire::ShadowedByProjectDecision
        }
    }
}

fn resolved_wire<T, U>(resolved: Resolved<T>, convert: impl FnOnce(T) -> U) -> ResolvedWire<U> {
    ResolvedWire {
        item: convert(resolved.item),
        origin: origin_wire(resolved.origin),
        layer: resolved.layer,
        reason: resolution_reason_wire(resolved.reason),
    }
}

fn evidence_category_wire(category: EvidenceCategory) -> EvidenceCategoryWire {
    match category {
        EvidenceCategory::Policy => EvidenceCategoryWire::Policy,
        EvidenceCategory::Decision => EvidenceCategoryWire::Decision,
        EvidenceCategory::Preference => EvidenceCategoryWire::Preference,
        EvidenceCategory::Directive => EvidenceCategoryWire::Directive,
    }
}

fn evidence_ref_wire(reference: EvidenceRef) -> EvidenceRefWire {
    EvidenceRefWire {
        category: evidence_category_wire(reference.category),
        id: reference.id,
        origin: origin_wire(reference.origin),
        scope: knowledge_scope_wire(reference.scope),
        layer: reference.layer,
        source_kind: reference.source_kind.map(source_kind_wire),
        status: reference.status.map(str::to_owned),
    }
}

fn conflict_kind_wire(kind: ConflictKind) -> ConflictKindWire {
    match kind {
        ConflictKind::SameSpecificityDecision => ConflictKindWire::SameSpecificityDecision,
        ConflictKind::SameSpecificityPreference => ConflictKindWire::SameSpecificityPreference,
        ConflictKind::ProtectedOverrideRejected => ConflictKindWire::ProtectedOverrideRejected,
        ConflictKind::InvalidProtectedProvenance => ConflictKindWire::InvalidProtectedProvenance,
    }
}

fn knowledge_conflict_wire(conflict: KnowledgeConflict) -> KnowledgeConflictWire {
    KnowledgeConflictWire {
        kind: conflict_kind_wire(conflict.kind),
        subject: conflict.subject,
        involved: conflict
            .involved
            .into_iter()
            .map(evidence_ref_wire)
            .collect(),
    }
}

fn work_item_source_kind_wire(kind: WorkItemSourceKind) -> WorkItemSourceKindWire {
    match kind {
        WorkItemSourceKind::Issue => WorkItemSourceKindWire::Issue,
        WorkItemSourceKind::ExternalTask => WorkItemSourceKindWire::ExternalTask,
        WorkItemSourceKind::UserRequest => WorkItemSourceKindWire::UserRequest,
    }
}

fn work_item_status_wire(status: WorkItemStatus) -> WorkItemStatusWire {
    match status {
        WorkItemStatus::Open => WorkItemStatusWire::Open,
        WorkItemStatus::Active => WorkItemStatusWire::Active,
        WorkItemStatus::Blocked => WorkItemStatusWire::Blocked,
        WorkItemStatus::Paused => WorkItemStatusWire::Paused,
        WorkItemStatus::Completed => WorkItemStatusWire::Completed,
        WorkItemStatus::Abandoned => WorkItemStatusWire::Abandoned,
    }
}

fn work_item_wire(item: WorkItem) -> WorkItemWire {
    WorkItemWire {
        uid: item.uid,
        source_kind: work_item_source_kind_wire(item.source_kind),
        source_ref: item.source_ref,
        title: item.title,
        goal: item.goal,
        status: work_item_status_wire(item.status),
        created_at: item.created_at,
        closed_at: item.closed_at,
    }
}

fn work_handoff_wire(handoff: WorkHandoff) -> WorkHandoffWire {
    WorkHandoffWire {
        work_item: handoff.work_item,
        handoff_summary: handoff.handoff_summary,
        remaining_summary: handoff.remaining_summary,
        blocker_summary: handoff.blocker_summary,
        next_scope_hint: handoff.next_scope_hint,
        created_at: handoff.created_at,
    }
}

fn work_result_status_wire(status: WorkResultStatus) -> WorkResultStatusWire {
    match status {
        WorkResultStatus::Completed => WorkResultStatusWire::Completed,
        WorkResultStatus::Partial => WorkResultStatusWire::Partial,
        WorkResultStatus::Abandoned => WorkResultStatusWire::Abandoned,
    }
}

fn dirty_observation_wire(
    observation: brainprint_engine::knowledge::DirtyObservation,
) -> DirtyObservationWire {
    use brainprint_engine::knowledge::DirtyObservation as D;
    match observation {
        D::Unknown => DirtyObservationWire::Unknown,
        D::Clean => DirtyObservationWire::Clean,
        D::Dirty { fingerprint } => DirtyObservationWire::Dirty { fingerprint },
    }
}

fn work_result_wire(result: WorkResult) -> WorkResultWire {
    WorkResultWire {
        work_item: result.work_item,
        result_status: work_result_status_wire(result.result_status),
        result_summary: result.result_summary,
        commit_id: result.commit_id,
        change_set_fingerprint: result.change_set_fingerprint,
        verification_summary: result.verification_summary,
        result_workspace_revision: result.result_workspace_revision,
        result_index_incarnation: result.result_index_incarnation,
        result_generation_no: result.result_generation_no,
        remaining_dirty: dirty_observation_wire(result.remaining_dirty),
        created_at: result.created_at,
    }
}

fn working_state_wire(state: WorkingState) -> WorkingStateWire {
    WorkingStateWire {
        work_item: state.work_item,
        baseline_workspace_revision: state.baseline_workspace_revision,
        baseline_index_incarnation: state.baseline_index_incarnation,
        baseline_generation_no: state.baseline_generation_no,
        baseline_head: state.baseline_head,
        baseline_dirty: dirty_observation_wire(state.baseline_dirty),
        current_step: state.current_step,
        progress_summary: state.progress_summary,
        remaining_summary: state.remaining_summary,
        blocker_summary: state.blocker_summary,
        owner_agent: state.owner_agent,
        last_observed_workspace_revision: state.last_observed_workspace_revision,
        updated_at: state.updated_at,
    }
}

fn work_resource_role_wire(role: WorkResourceRole) -> WorkResourceRoleWire {
    match role {
        WorkResourceRole::Target => WorkResourceRoleWire::Target,
        WorkResourceRole::Touched => WorkResourceRoleWire::Touched,
        WorkResourceRole::Owned => WorkResourceRoleWire::Owned,
        WorkResourceRole::Related => WorkResourceRoleWire::Related,
        WorkResourceRole::PreexistingDirty => WorkResourceRoleWire::PreexistingDirty,
    }
}

fn work_overlap_wire(overlap: WorkOverlap) -> WorkOverlapWire {
    WorkOverlapWire {
        other: overlap.other,
        other_status: work_item_status_wire(overlap.other_status),
        resource: overlap.resource,
        this_roles: overlap
            .this_roles
            .into_iter()
            .map(work_resource_role_wire)
            .collect(),
        other_roles: overlap
            .other_roles
            .into_iter()
            .map(work_resource_role_wire)
            .collect(),
    }
}

fn generation_reference_state_wire(
    state: GenerationReferenceState,
) -> GenerationReferenceStateWire {
    match state {
        GenerationReferenceState::PresentMatching => GenerationReferenceStateWire::PresentMatching,
        GenerationReferenceState::HistoricalMissingOrReused => {
            GenerationReferenceStateWire::HistoricalMissingOrReused
        }
    }
}

fn generation_reference_wire(reference: GenerationReference) -> GenerationReferenceWire {
    GenerationReferenceWire {
        index_incarnation: reference.index_incarnation,
        generation_no: reference.generation_no,
        workspace_revision: reference.workspace_revision,
        state: generation_reference_state_wire(reference.state),
    }
}

fn staleness_wire(staleness: Staleness) -> StalenessWire {
    match staleness {
        Staleness::NotEvaluated => StalenessWire::NotEvaluated,
        Staleness::CurrentAtCutoff => StalenessWire::CurrentAtCutoff,
        Staleness::PossiblyStale => StalenessWire::PossiblyStale,
        Staleness::Closed => StalenessWire::Closed,
    }
}

fn generation_basis_wire(basis: GenerationBasis) -> GenerationBasisWire {
    match basis {
        GenerationBasis::Baseline => GenerationBasisWire::Baseline,
        GenerationBasis::Result => GenerationBasisWire::Result,
    }
}

pub fn knowledge_result_wire(result: KnowledgeResult) -> KnowledgeResultWire {
    match result {
        KnowledgeResult::Rules { evidence, gaps } => KnowledgeResultWire::Rules {
            evidence: evidence.into_iter().map(evidence_item_wire).collect(),
            gaps: gaps.into_iter().map(projection_gap_wire).collect(),
        },
        KnowledgeResult::WorkItems { items, truncated } => KnowledgeResultWire::WorkItems {
            items: items.into_iter().map(work_item_wire).collect(),
            truncated,
        },
        KnowledgeResult::PolicyLineage(lineage) => {
            KnowledgeResultWire::PolicyLineage(policy_lineage_wire(lineage))
        }
        KnowledgeResult::DecisionLineage(lineage) => {
            KnowledgeResultWire::DecisionLineage(decision_lineage_wire(lineage))
        }
        KnowledgeResult::Handoffs {
            work_item,
            handoffs,
            truncated,
        } => KnowledgeResultWire::Handoffs {
            work_item,
            handoffs: handoffs.into_iter().map(work_handoff_wire).collect(),
            truncated,
        },
    }
}

// -------------------------------------------------------------- evidence

fn impact_intent_wire(intent: ImpactIntent) -> ImpactIntentWire {
    match intent {
        ImpactIntent::PublicSignatureChange => ImpactIntentWire::PublicSignatureChange,
        ImpactIntent::Rename => ImpactIntentWire::Rename,
        ImpactIntent::ModuleMove => ImpactIntentWire::ModuleMove,
        ImpactIntent::BaseInterfaceChange => ImpactIntentWire::BaseInterfaceChange,
    }
}

fn change_kind_wire(kind: ChangeKind) -> ChangeKindWire {
    match kind {
        ChangeKind::Structural(intent) => ChangeKindWire::Structural(impact_intent_wire(intent)),
        ChangeKind::Delete => ChangeKindWire::Delete,
        ChangeKind::DomainContractChange => ChangeKindWire::DomainContractChange,
    }
}

fn target_resolution_wire(resolution: TargetResolution) -> TargetResolutionWire {
    match resolution {
        TargetResolution::Resolved(endpoint) => {
            TargetResolutionWire::Resolved(graph_endpoint_wire(endpoint))
        }
        TargetResolution::MultipleCandidates => TargetResolutionWire::MultipleCandidates,
        TargetResolution::SingleNonExactCandidate => TargetResolutionWire::SingleNonExactCandidate,
        TargetResolution::NotFound => TargetResolutionWire::NotFound,
        TargetResolution::NotFoundIncompleteCoverage => {
            TargetResolutionWire::NotFoundIncompleteCoverage
        }
        TargetResolution::NotCurrent => TargetResolutionWire::NotCurrent,
        TargetResolution::NoTarget => TargetResolutionWire::NoTarget,
    }
}

fn target_selection_wire(selection: TargetSelection) -> TargetSelectionWire {
    TargetSelectionWire {
        selector: projection_target_wire(selection.selector),
        candidates: selection
            .located
            .candidates
            .into_iter()
            .map(graph_endpoint_wire)
            .collect(),
        last_valid: selection
            .located
            .last_valid
            .into_iter()
            .map(graph_endpoint_wire)
            .collect(),
        exact_selector: selection.located.exact_selector,
        truncated: selection.located.truncated,
        currentness: currentness_wire(selection.located.currentness),
        source: result_source_wire(selection.located.source),
        incomplete_coverage: selection
            .located
            .incomplete_coverage
            .into_iter()
            .map(coverage_note_wire)
            .collect(),
    }
}

fn coverage_limit_wire(limit: CoverageLimit) -> CoverageLimitWire {
    match limit {
        CoverageLimit::UnresolvedEvidence => CoverageLimitWire::UnresolvedEvidence,
        CoverageLimit::AmbiguousCandidates => CoverageLimitWire::AmbiguousCandidates,
        CoverageLimit::RequiresSemantics => CoverageLimitWire::RequiresSemantics,
        CoverageLimit::UnsupportedConstruct => CoverageLimitWire::UnsupportedConstruct,
        CoverageLimit::CandidateTruncated => CoverageLimitWire::CandidateTruncated,
        CoverageLimit::UnattributedGaps => CoverageLimitWire::UnattributedGaps,
        CoverageLimit::ReverseScopeNotEnumerable => CoverageLimitWire::ReverseScopeNotEnumerable,
        CoverageLimit::PartialSupport => CoverageLimitWire::PartialSupport,
        CoverageLimit::UnsupportedScope => CoverageLimitWire::UnsupportedScope,
        CoverageLimit::StaleEvidence => CoverageLimitWire::StaleEvidence,
        CoverageLimit::DirtyRelationComponent => CoverageLimitWire::DirtyRelationComponent,
        CoverageLimit::TraversalTruncated => CoverageLimitWire::TraversalTruncated,
        CoverageLimit::SupportingPathTruncated => CoverageLimitWire::SupportingPathTruncated,
        CoverageLimit::UnknownResourceRole => CoverageLimitWire::UnknownResourceRole,
        CoverageLimit::UnreadableResourceOwner => CoverageLimitWire::UnreadableResourceOwner,
        CoverageLimit::IndexNotCurrent => CoverageLimitWire::IndexNotCurrent,
        CoverageLimit::SemanticConflict => CoverageLimitWire::SemanticConflict,
        CoverageLimit::SemanticNotCurrent => CoverageLimitWire::SemanticNotCurrent,
    }
}

fn coverage_report_wire(report: CoverageReport) -> CoverageReportWire {
    CoverageReportWire {
        limits: report
            .limits()
            .iter()
            .copied()
            .map(coverage_limit_wire)
            .collect(),
    }
}

fn answer_state_wire(state: AnswerState) -> AnswerStateWire {
    match state {
        AnswerState::Confirmed => AnswerStateWire::Confirmed,
        AnswerState::NoneUnderCompleteCoverage => AnswerStateWire::NoneUnderCompleteCoverage,
        AnswerState::NoneWithIncompleteCoverage => AnswerStateWire::NoneWithIncompleteCoverage,
    }
}

fn coverage_subject_wire(subject: CoverageSubject) -> CoverageSubjectWire {
    match subject {
        CoverageSubject::TargetSelection(target) => {
            CoverageSubjectWire::TargetSelection(projection_target_wire(target))
        }
        CoverageSubject::Relations {
            anchor,
            direction,
            kinds,
        } => CoverageSubjectWire::Relations {
            anchor: graph_endpoint_wire(anchor),
            direction: direction_wire(direction),
            kinds: kinds.into_iter().map(relation_kind_wire).collect(),
        },
        CoverageSubject::Impact { root, intent } => CoverageSubjectWire::Impact {
            root: graph_endpoint_wire(root),
            intent: impact_intent_wire(intent),
        },
        CoverageSubject::RelatedTests { target, intent } => CoverageSubjectWire::RelatedTests {
            target: graph_endpoint_wire(target),
            intent: impact_intent_wire(intent),
        },
    }
}

fn coverage_evidence_wire(evidence: CoverageEvidence) -> CoverageEvidenceWire {
    let answer_state = evidence.answer_state();
    CoverageEvidenceWire {
        subject: coverage_subject_wire(evidence.subject),
        report: coverage_report_wire(evidence.report),
        confirmed: evidence.confirmed,
        answer_state: answer_state_wire(answer_state),
    }
}

fn source_unavailable_wire(reason: SourceUnavailable) -> SourceUnavailableWire {
    match reason {
        SourceUnavailable::StaleBasis {
            basis_revision,
            current_revision,
        } => SourceUnavailableWire::StaleBasis {
            basis_revision,
            current_revision,
        },
        SourceUnavailable::SourceChanged {
            expected_content_hash,
            observed_content_hash,
        } => SourceUnavailableWire::SourceChanged {
            expected_content_hash,
            observed_content_hash,
        },
        SourceUnavailable::SymbolNotCurrent {
            symbol_revision,
            resource_revision,
        } => SourceUnavailableWire::SymbolNotCurrent {
            symbol_revision,
            resource_revision,
        },
        SourceUnavailable::NoCurrentSource { detail } => {
            SourceUnavailableWire::NoCurrentSource { detail }
        }
        SourceUnavailable::SpanNotReadable { detail } => {
            SourceUnavailableWire::SpanNotReadable { detail }
        }
    }
}

fn range_role_wire(role: RangeRole) -> RangeRoleWire {
    match role {
        RangeRole::EvidenceSpan => RangeRoleWire::EvidenceSpan,
        RangeRole::ContainingDeclaration => RangeRoleWire::ContainingDeclaration,
        RangeRole::AnchorDeclaration => RangeRoleWire::AnchorDeclaration,
    }
}

fn source_verification_wire(verification: SourceVerification) -> SourceVerificationWire {
    SourceVerificationWire {
        expected_content_hash: verification.expected_content_hash,
        observed_content_hash: verification.observed_content_hash,
        currentness: currentness_wire(verification.currentness),
    }
}

fn prepared_range_wire(range: PreparedRange) -> PreparedRangeWire {
    PreparedRangeWire {
        resource: range.resource,
        path_rel: range.path_rel,
        resource_revision: range.resource_revision,
        span: source_span_wire(range.span),
        source: range.source,
        role: range_role_wire(range.role),
        verification: source_verification_wire(range.verification),
    }
}

fn projection_gap_wire(gap: ProjectionGap) -> ProjectionGapWire {
    match gap {
        ProjectionGap::TargetAmbiguous => ProjectionGapWire::TargetAmbiguous,
        ProjectionGap::TargetNotExact => ProjectionGapWire::TargetNotExact,
        ProjectionGap::TargetNotFound => ProjectionGapWire::TargetNotFound,
        ProjectionGap::TargetNotFoundWithIncompleteCoverage => {
            ProjectionGapWire::TargetNotFoundWithIncompleteCoverage
        }
        ProjectionGap::TargetNotCurrent => ProjectionGapWire::TargetNotCurrent,
        ProjectionGap::UnsupportedImpactProfile(kind) => {
            ProjectionGapWire::UnsupportedImpactProfile(change_kind_wire(kind))
        }
        ProjectionGap::DependencyExpansionUndefined => {
            ProjectionGapWire::DependencyExpansionUndefined
        }
        ProjectionGap::BlueprintApplicationNotApplied(id) => {
            ProjectionGapWire::BlueprintApplicationNotApplied(id)
        }
        ProjectionGap::RequiresSemantics => ProjectionGapWire::RequiresSemantics,
        ProjectionGap::NotCurrent => ProjectionGapWire::NotCurrent,
    }
}

fn evidence_item_wire(item: EvidenceItem) -> EvidenceWire {
    match item {
        EvidenceItem::Resource(resource) => EvidenceWire::Resource(resource_wire(resource)),
        EvidenceItem::Symbol(candidate) => EvidenceWire::Symbol(symbol_candidate_wire(candidate)),
        EvidenceItem::Relation(result) => EvidenceWire::Relation(relation_result_wire(result)),
        EvidenceItem::RelationGap(gap) => EvidenceWire::RelationGap(relation_gap_wire(gap)),
        EvidenceItem::Policy(resolved) => {
            EvidenceWire::Policy(resolved_wire(resolved, policy_wire))
        }
        EvidenceItem::Decision(resolved) => {
            EvidenceWire::Decision(resolved_wire(resolved, decision_wire))
        }
        EvidenceItem::Preference(resolved) => {
            EvidenceWire::Preference(resolved_wire(resolved, user_preference_wire))
        }
        EvidenceItem::Blueprint(resolved) => {
            EvidenceWire::Blueprint(resolved_wire(resolved, blueprint_evidence_wire))
        }
        EvidenceItem::ProjectState(resolved) => {
            EvidenceWire::ProjectState(resolved_wire(resolved, project_state_wire))
        }
        EvidenceItem::WorkItem(item) => EvidenceWire::WorkItem(work_item_wire(item)),
        EvidenceItem::WorkingState(state) => EvidenceWire::WorkingState(working_state_wire(state)),
        EvidenceItem::WorkResult(result) => EvidenceWire::WorkResult(work_result_wire(result)),
        EvidenceItem::Handoff(handoff) => EvidenceWire::Handoff(work_handoff_wire(handoff)),
        EvidenceItem::CurrentSource(range) => {
            EvidenceWire::CurrentSource(prepared_range_wire(range))
        }
        EvidenceItem::Directive(resolved) => {
            EvidenceWire::Directive(resolved_wire(resolved, request_directive_wire))
        }
        EvidenceItem::RelatedTest { target, candidate } => EvidenceWire::RelatedTest {
            target: graph_endpoint_wire(target),
            candidate: related_test_candidate_wire(candidate),
        },
        EvidenceItem::KnowledgeConflict(conflict) => {
            EvidenceWire::KnowledgeConflict(knowledge_conflict_wire(conflict))
        }
        EvidenceItem::WorkOverlap { work_item, overlap } => EvidenceWire::WorkOverlap {
            work_item,
            overlap: work_overlap_wire(overlap),
        },
        EvidenceItem::GenerationReference {
            work_item,
            basis,
            reference,
        } => EvidenceWire::GenerationReference {
            work_item,
            basis: generation_basis_wire(basis),
            reference: generation_reference_wire(reference),
        },
        EvidenceItem::WorkStaleness {
            work_item,
            staleness,
        } => EvidenceWire::WorkStaleness {
            work_item,
            staleness: staleness_wire(staleness),
        },
        EvidenceItem::TargetSelection(selection) => {
            EvidenceWire::TargetSelection(target_selection_wire(selection))
        }
        EvidenceItem::Coverage(evidence) => {
            EvidenceWire::Coverage(coverage_evidence_wire(evidence))
        }
        EvidenceItem::SourceUnavailable {
            resource,
            span,
            reason,
        } => EvidenceWire::SourceUnavailable {
            resource,
            span: source_span_wire(span),
            reason: source_unavailable_wire(reason),
        },
        EvidenceItem::IndexCurrentness {
            workspace,
            currentness,
        } => EvidenceWire::IndexCurrentness {
            workspace,
            currentness: currentness_wire(currentness),
        },
    }
}

// -------------------------------------------------------------- delivery

fn measure_wire(measure: Measure) -> MeasureWire {
    match measure {
        Measure::Known(value) => MeasureWire::Known(value),
        Measure::Unknown => MeasureWire::Unknown,
        Measure::NotMeasured => MeasureWire::NotMeasured,
    }
}

fn stage_amount_wire(
    amount: brainprint_engine::projection::planner::StageAmount,
) -> StageAmountWire {
    StageAmountWire {
        items: measure_wire(amount.items),
        bytes: measure_wire(amount.bytes),
        tokens: measure_wire(amount.tokens),
    }
}

fn delivery_dimension_wire(dimension: DeliveryDimension) -> DeliveryDimensionWire {
    match dimension {
        DeliveryDimension::Items => DeliveryDimensionWire::Items,
        DeliveryDimension::Bytes => DeliveryDimensionWire::Bytes,
        DeliveryDimension::Tokens => DeliveryDimensionWire::Tokens,
    }
}

fn continuation_unavailable_wire(reason: ContinuationUnavailable) -> ContinuationUnavailableWire {
    match reason {
        ContinuationUnavailable::NoStableGeneration => {
            ContinuationUnavailableWire::NoStableGeneration
        }
        ContinuationUnavailable::UnitExceedsBudget => {
            ContinuationUnavailableWire::UnitExceedsBudget
        }
        ContinuationUnavailable::OptionalSourceTokenCostUnknown => {
            ContinuationUnavailableWire::OptionalSourceTokenCostUnknown
        }
    }
}

fn reuse_identity_wire(identity: ReuseIdentity) -> ReuseIdentityWire {
    match identity {
        ReuseIdentity::Resource(id) => ReuseIdentityWire::Resource(id),
        ReuseIdentity::Symbol(id) => ReuseIdentityWire::Symbol(id),
        ReuseIdentity::Relation {
            kind,
            source,
            target,
        } => ReuseIdentityWire::Relation {
            kind: relation_kind_wire(kind),
            source: graph_endpoint_wire(source),
            target: graph_endpoint_wire(target),
        },
        ReuseIdentity::Policy(id) => ReuseIdentityWire::Policy(id),
        ReuseIdentity::Decision(id) => ReuseIdentityWire::Decision(id),
        ReuseIdentity::Preference(id) => ReuseIdentityWire::Preference(id),
        ReuseIdentity::Blueprint(id) => ReuseIdentityWire::Blueprint(id),
        ReuseIdentity::ProjectState(id) => ReuseIdentityWire::ProjectState(id),
        ReuseIdentity::WorkItem(id) => ReuseIdentityWire::WorkItem(id),
        ReuseIdentity::WorkingState(id) => ReuseIdentityWire::WorkingState(id),
        ReuseIdentity::WorkResult(id) => ReuseIdentityWire::WorkResult(id),
        ReuseIdentity::Handoff(id) => ReuseIdentityWire::Handoff(id),
        ReuseIdentity::CurrentSource {
            resource,
            resource_revision,
            span,
            role,
        } => ReuseIdentityWire::CurrentSource {
            resource,
            resource_revision,
            span: source_span_wire(span),
            role: range_role_wire(role),
        },
        ReuseIdentity::RelatedTest { target, test } => ReuseIdentityWire::RelatedTest {
            target: graph_endpoint_wire(target),
            test,
        },
    }
}

fn reuse_reference_wire(reference: ReuseReference) -> ReuseReferenceWire {
    ReuseReferenceWire {
        identity: reuse_identity_wire(reference.identity),
        version: hex32(reference.version),
    }
}

fn delivery_page_wire(page: DeliveryPage) -> DeliveryPageWire {
    DeliveryPageWire {
        evidence: page.evidence.into_iter().map(evidence_item_wire).collect(),
        references: page
            .references
            .into_iter()
            .map(|reference| reference.map(reuse_reference_wire))
            .collect(),
        gaps: page.gaps.into_iter().map(projection_gap_wire).collect(),
        used_items: page.used_items,
        used_bytes: page.used_bytes,
    }
}

fn delivery_economy_wire(economy: ProjectionEconomy) -> DeliveryEconomyWire {
    DeliveryEconomyWire {
        raw_available: stage_amount_wire(economy.raw_available),
        prepared: stage_amount_wire(economy.prepared),
        delivered: stage_amount_wire(economy.delivered),
        omitted_items: economy.omitted_items,
        more_available: economy.more_available,
        limiting: economy
            .limiting
            .into_iter()
            .map(delivery_dimension_wire)
            .collect(),
        continuation_unavailable: economy
            .continuation_unavailable
            .map(continuation_unavailable_wire),
    }
}

fn delivery_key_wire(key: &brainprint_engine::projection::planner::DeliveryKey) -> DeliveryKeyWire {
    DeliveryKeyWire {
        tier: key.tier(),
        depth: key.depth(),
        identity: hex32(key.identity()),
    }
}

fn continuation_budget_wire(
    budget: brainprint_engine::projection::planner::DeliveryBudget,
) -> ContinuationBudgetWire {
    ContinuationBudgetWire {
        max_items: budget.max_items(),
        max_bytes: budget.max_bytes(),
        max_tokens: budget.max_tokens(),
    }
}

fn delivery_continuation_wire(
    continuation: brainprint_engine::projection::planner::DeliveryContinuation,
) -> DeliveryContinuationWire {
    DeliveryContinuationWire {
        workspace_id: continuation.workspace(),
        index_incarnation_id: continuation.index_incarnation(),
        workspace_revision: continuation.workspace_revision().to_owned(),
        generation_no: continuation.generation_no(),
        generation_basis_revision: continuation.generation_basis_revision().to_owned(),
        request_fingerprint: hex32(continuation.request_fingerprint()),
        projection_fingerprint: hex32(continuation.projection_fingerprint()),
        budget: continuation_budget_wire(continuation.budget()),
        next: delivery_key_wire(continuation.next()),
    }
}

/// Split a [`ProjectedAnswer`] into its wire form and the Task 8
/// [`brainprint_engine::projection::planner::DeliveryReceipt`] the caller
/// (the Workspace worker) stores under a fresh `ack_token` -- the receipt
/// itself never reaches the wire.
pub fn projected_answer_wire(
    answer: ProjectedAnswer,
) -> (
    ProjectedAnswerWire,
    brainprint_engine::projection::planner::DeliveryReceipt,
) {
    let PendingDelivery {
        page,
        economy,
        receipt,
    } = answer.delivery;
    let more_available = page.more_available;
    let continuation = page.continuation.clone().map(delivery_continuation_wire);
    let wire = ProjectedAnswerWire {
        target_resolution: target_resolution_wire(answer.target),
        currentness: currentness_wire(answer.currentness),
        page: delivery_page_wire(page),
        economy: delivery_economy_wire(economy),
        continuation,
        more_available,
    };
    (wire, receipt)
}

// ------------------------------------------------------------------ find

fn file_listing_wire(listing: FileListing) -> FileListingWire {
    FileListingWire {
        entries: listing.entries.into_iter().map(resource_wire).collect(),
        truncated: listing.truncated,
        currentness: currentness_wire(listing.currentness),
        source: result_source_wire(listing.source),
    }
}

fn query_status_wire(status: QueryStatus) -> QueryStatusWire {
    match status {
        QueryStatus::Found => QueryStatusWire::Found,
        QueryStatus::NotFound => QueryStatusWire::NotFound,
        QueryStatus::Ambiguous => QueryStatusWire::Ambiguous,
        QueryStatus::Unsupported => QueryStatusWire::Unsupported,
        QueryStatus::Truncated => QueryStatusWire::Truncated,
        QueryStatus::Refreshing => QueryStatusWire::Refreshing,
        QueryStatus::Unavailable => QueryStatusWire::Unavailable,
    }
}

fn match_source_wire(source: MatchSource) -> MatchSourceWire {
    match source {
        MatchSource::TextFallback => MatchSourceWire::TextFallback,
    }
}

fn fallback_reason_wire(reason: FallbackReason) -> FallbackReasonWire {
    match reason {
        FallbackReason::ExplicitTextSearch => FallbackReasonWire::ExplicitTextSearch,
        FallbackReason::NonStructuralTarget => FallbackReasonWire::NonStructuralTarget,
        FallbackReason::IncompleteStructuralCoverage => {
            FallbackReasonWire::IncompleteStructuralCoverage
        }
    }
}

fn budget_axis_wire(axis: BudgetAxis) -> BudgetAxisWire {
    match axis {
        BudgetAxis::Results => BudgetAxisWire::Results,
        BudgetAxis::Files => BudgetAxisWire::Files,
        BudgetAxis::Bytes => BudgetAxisWire::Bytes,
        BudgetAxis::Deadline => BudgetAxisWire::Deadline,
    }
}

fn scope_report_wire(report: ScopeReport) -> ScopeReportWire {
    ScopeReportWire {
        files_scanned: report.files_scanned,
        bytes_scanned: report.bytes_scanned,
        binary_skipped: report.binary_skipped,
        oversized_skipped: report.oversized_skipped,
        unreadable: report.unreadable,
        changed_during_scan: report.changed_during_scan,
        budget_exhausted: report.budget_exhausted.map(budget_axis_wire),
    }
}

fn text_match_wire(text_match: TextMatch) -> TextMatchWire {
    TextMatchWire {
        path_rel: text_match.path_rel,
        resource_id: text_match.resource_id,
        span: source_span_wire(text_match.span),
        preview: text_match.preview,
        source: match_source_wire(text_match.source),
    }
}

fn text_search_result_wire(result: TextSearchResult) -> TextSearchResultWire {
    TextSearchResultWire {
        status: query_status_wire(result.status),
        matches: result.matches.into_iter().map(text_match_wire).collect(),
        scope: scope_report_wire(result.scope),
        structural_currentness: currentness_wire(result.structural_currentness),
        reason: fallback_reason_wire(result.reason),
    }
}

/// Splits a [`FindResult`] into its wire form and, for `Target`, the
/// pending Task 8 receipt (see [`projected_answer_wire`]).
pub fn find_result_wire(
    result: FindResult,
) -> (
    FindResultWire,
    Option<brainprint_engine::projection::planner::DeliveryReceipt>,
) {
    match result {
        FindResult::Target(answer) => {
            let (wire, receipt) = projected_answer_wire(answer);
            (FindResultWire::Target(wire), Some(receipt))
        }
        FindResult::Files(listing) => (FindResultWire::Files(file_listing_wire(listing)), None),
        FindResult::Text(result) => (FindResultWire::Text(text_search_result_wire(result)), None),
    }
}

// -------------------------------------------------------------- relations

pub fn relations_result_wire(result: RelationsResult) -> RelationsResultWire {
    RelationsResultWire {
        target: target_resolution_wire(result.target),
        selection: result
            .selection
            .into_iter()
            .map(evidence_item_wire)
            .collect(),
        currentness: currentness_wire(result.currentness),
        answers: result
            .answers
            .into_iter()
            .map(relation_answer_wire)
            .collect(),
    }
}

// -------------------------------------------------------------- structure

fn group_basis_wire(basis: GroupBasis) -> GroupBasisWire {
    match basis {
        GroupBasis::PathPrefix => GroupBasisWire::PathPrefix,
        GroupBasis::DirectoryDepth => GroupBasisWire::DirectoryDepth,
        GroupBasis::ResourceRole => GroupBasisWire::ResourceRole,
        GroupBasis::ResourceLanguage => GroupBasisWire::ResourceLanguage,
        GroupBasis::ResourceKind => GroupBasisWire::ResourceKind,
    }
}

fn structural_group_wire(group: StructuralGroup) -> StructuralGroupWire {
    match group {
        StructuralGroup::Group(label) => StructuralGroupWire::Group(label),
        StructuralGroup::Ungrouped => StructuralGroupWire::Ungrouped,
    }
}

fn group_coverage_wire(coverage: GroupCoverage) -> GroupCoverageWire {
    GroupCoverageWire {
        supported: coverage.supported,
        partial: coverage.partial,
        unsupported: coverage.unsupported,
        structure_not_current: coverage.structure_not_current,
        relation_dirty: coverage.relation_dirty,
        unresolved_gaps: coverage.unresolved_gaps,
        candidate_gaps: coverage.candidate_gaps,
        requires_semantics: coverage.requires_semantics,
        unsupported_construct: coverage.unsupported_construct,
        candidate_truncated: coverage.candidate_truncated,
        semantic_conflicts: coverage.semantic_conflicts,
        semantic_not_current: coverage.semantic_not_current,
    }
}

fn member_sample_wire(sample: MemberSample) -> MemberSampleWire {
    MemberSampleWire {
        resource: sample.resource,
        path: sample.path,
        role: resource_role_wire(sample.role),
        language: sample.language.map(resource_language_wire),
        kind: resource_kind_wire(sample.kind),
    }
}

fn group_summary_wire(summary: GroupSummary) -> GroupSummaryWire {
    GroupSummaryWire {
        group: structural_group_wire(summary.group),
        prefixes: summary.prefixes,
        resources: summary.resources,
        internal_edges: summary.internal_edges,
        outgoing_edges: summary.outgoing_edges,
        incoming_edges: summary.incoming_edges,
        fan_out_groups: summary.fan_out_groups,
        fan_in_groups: summary.fan_in_groups,
        coverage: group_coverage_wire(summary.coverage),
        members: summary
            .members
            .into_iter()
            .map(member_sample_wire)
            .collect(),
    }
}

fn boundary_edge_wire(edge: BoundaryEdge) -> BoundaryEdgeWire {
    BoundaryEdgeWire {
        source: structural_group_wire(edge.source),
        target: structural_group_wire(edge.target),
        kind: relation_kind_wire(edge.kind),
        confirmed_edges: edge.confirmed_edges,
    }
}

fn gap_aggregate_wire(gap: GapAggregate) -> GapAggregateWire {
    GapAggregateWire {
        group: structural_group_wire(gap.group),
        intended: intended_relation_wire(gap.intended),
        reason: unresolved_reason_wire(gap.reason),
        unresolved: gap.unresolved,
        candidate: gap.candidate,
        candidate_truncated: gap.candidate_truncated,
    }
}

pub fn structural_summary_wire(summary: StructuralSummary) -> StructuralSummaryWire {
    StructuralSummaryWire {
        basis: group_basis_wire(summary.basis),
        relation_kinds: summary
            .relation_kinds
            .into_iter()
            .map(relation_kind_wire)
            .collect(),
        currentness: currentness_wire(summary.currentness),
        groups: summary.groups.into_iter().map(group_summary_wire).collect(),
        boundary_edges: summary
            .boundary_edges
            .into_iter()
            .map(boundary_edge_wire)
            .collect(),
        cycles: summary.cycles.map(|cycles| {
            cycles
                .into_iter()
                .map(|cycle| cycle.into_iter().map(structural_group_wire).collect())
                .collect()
        }),
        gaps: summary.gaps.into_iter().map(gap_aggregate_wire).collect(),
    }
}

// ---------------------------------------------------------------- error

/// Foreground/debug stderr only -- never sent to a client (matches
/// `handlers::log_internal_error`'s pattern).
fn log_internal(operation: &str, error: &dyn std::error::Error) {
    eprintln!("brainprintd: query {operation} failed: {error}");
}

fn not_initialized_wire(reason: NotInitialized) -> NotInitializedReasonWire {
    match reason {
        NotInitialized::GlobalDbMissing => NotInitializedReasonWire::GlobalDbMissing,
        NotInitialized::WorkspaceNotRegistered => NotInitializedReasonWire::WorkspaceNotRegistered,
        NotInitialized::WorkspaceDbMissing => NotInitializedReasonWire::WorkspaceDbMissing,
        NotInitialized::IndexDbMissing => NotInitializedReasonWire::IndexDbMissing,
        NotInitialized::WorkspaceUnbound { db } => {
            NotInitializedReasonWire::WorkspaceUnbound { db: db.to_owned() }
        }
    }
}

fn invalid_request_reason_wire(reason: &InvalidRequest) -> InvalidRequestReasonWire {
    use brainprint_engine::projection::ProjectionRequestError as P;
    match reason {
        InvalidRequest::Projection(error) => match error {
            P::MissingTarget => InvalidRequestReasonWire::MissingTarget,
            P::MissingWorkItem => InvalidRequestReasonWire::MissingWorkItem,
            P::EmptySelector => InvalidRequestReasonWire::EmptySelector,
            P::InvalidScopeLayer(_) => InvalidRequestReasonWire::InvalidScopeLayer,
            P::DirectiveOutsideApplicability { .. } => {
                InvalidRequestReasonWire::DirectiveOutsideApplicability
            }
            P::InvalidDirective(_) => InvalidRequestReasonWire::InvalidDirective,
            P::EmptyCorrelationId(_) => InvalidRequestReasonWire::EmptyCorrelationId,
            P::EmptyKnowledgeRef(_) => InvalidRequestReasonWire::EmptyKnowledgeRef,
        },
        InvalidRequest::Summary(_) => InvalidRequestReasonWire::InvalidSummaryRequest,
        InvalidRequest::ListLimitTooLarge { limit, max } => {
            InvalidRequestReasonWire::ListLimitTooLarge {
                limit: *limit,
                max: *max,
            }
        }
        InvalidRequest::SearchBudgetInvalid => InvalidRequestReasonWire::SearchBudgetInvalid,
    }
}

fn continuation_mismatch_wire(
    mismatch: brainprint_engine::projection::planner::ContinuationMismatch,
) -> ContinuationMismatchReasonWire {
    use brainprint_engine::projection::planner::ContinuationMismatch as M;
    match mismatch {
        M::Workspace => ContinuationMismatchReasonWire::Workspace,
        M::IndexIncarnation => ContinuationMismatchReasonWire::IndexIncarnation,
        M::WorkspaceRevision => ContinuationMismatchReasonWire::WorkspaceRevision,
        M::StableGeneration => ContinuationMismatchReasonWire::StableGeneration,
        M::Request => ContinuationMismatchReasonWire::Request,
        M::Projection => ContinuationMismatchReasonWire::Projection,
        M::Budget => ContinuationMismatchReasonWire::Budget,
    }
}

/// The complete #24 §9 `CoreError` -> wire mapping.
pub fn core_error(error: CoreError) -> QueryErrorWire {
    match error {
        CoreError::NotInitialized(reason) => QueryErrorWire {
            code: QueryErrorCodeWire::NotInitialized,
            detail: Some(QueryErrorDetailWire::NotInitialized(not_initialized_wire(
                reason,
            ))),
            message: "this Workspace is not initialized".to_owned(),
        },
        CoreError::WorkspaceRootMissing { workspace } => QueryErrorWire {
            code: QueryErrorCodeWire::WorkspaceRootMissing,
            detail: None,
            message: format!("the root of Workspace {workspace} does not exist"),
        },
        CoreError::WorkspaceLocatorAmbiguous { workspaces } => QueryErrorWire {
            code: QueryErrorCodeWire::WorkspaceAmbiguous,
            detail: Some(QueryErrorDetailWire::WorkspaceAmbiguous {
                workspaces: workspaces.clone(),
            }),
            message: format!(
                "{} Workspaces are registered at this locator",
                workspaces.len()
            ),
        },
        CoreError::WorkspaceMismatch { bound, requested } => QueryErrorWire {
            code: QueryErrorCodeWire::WorkspaceMismatch,
            detail: Some(QueryErrorDetailWire::WorkspaceMismatch { bound, requested }),
            message: format!("bound to Workspace {bound}, request names {requested}"),
        },
        CoreError::WorkspaceBindingMismatch {
            db,
            expected,
            found,
        } => QueryErrorWire {
            code: QueryErrorCodeWire::WorkspaceBindingMismatch,
            detail: Some(QueryErrorDetailWire::WorkspaceBindingMismatch {
                db: db.to_owned(),
                expected,
                found,
            }),
            message: format!("{db} belongs to Workspace {found}, not {expected}"),
        },
        CoreError::WorkItemNotFound { work_item } => QueryErrorWire {
            code: QueryErrorCodeWire::WorkItemNotFound,
            detail: Some(QueryErrorDetailWire::WorkItemNotFound { work_item }),
            message: format!("WorkItem {work_item} is not in this Workspace"),
        },
        CoreError::InvalidRequest(reason) => {
            let detail = invalid_request_reason_wire(&reason);
            QueryErrorWire {
                code: QueryErrorCodeWire::InvalidRequest,
                detail: Some(QueryErrorDetailWire::InvalidRequest(detail)),
                message: "invalid request".to_owned(),
            }
        }
        CoreError::Delivery(error) => delivery_error_wire(error),
        CoreError::Planner(error) => {
            log_internal("planner", &error);
            query_failed("planner")
        }
        CoreError::Relation(error) => {
            log_internal("relation", &error);
            query_failed("relation")
        }
        CoreError::Knowledge(error) => {
            log_internal("knowledge", &error);
            query_failed("knowledge")
        }
        CoreError::Work(error) => {
            log_internal("work", &error);
            query_failed("work")
        }
        CoreError::Query(error) => {
            log_internal("query", &error);
            query_failed("query")
        }
        CoreError::Search(error) => {
            log_internal("search", &error);
            query_failed("search")
        }
        CoreError::Summary(error) => {
            log_internal("summary", &error);
            query_failed("summary")
        }
        CoreError::Registry(error) => {
            log_internal("registry", &error);
            query_failed("registry")
        }
        CoreError::Config(error) => {
            log_internal("config", &error);
            query_failed("config")
        }
    }
}

fn query_failed(category: &str) -> QueryErrorWire {
    QueryErrorWire {
        code: QueryErrorCodeWire::QueryFailed,
        detail: None,
        message: format!("{category} query failed; see daemon logs for detail"),
    }
}

fn delivery_error_wire(
    error: brainprint_engine::projection::planner::DeliveryError,
) -> QueryErrorWire {
    use brainprint_engine::projection::planner::DeliveryError as D;
    match error {
        D::BudgetTooSmallForRequiredEvidence { .. } => QueryErrorWire {
            code: QueryErrorCodeWire::BudgetTooSmall,
            detail: None,
            message: "budget too small for the required evidence bundle".to_owned(),
        },
        D::ContinuationMismatch(mismatch) => QueryErrorWire {
            code: QueryErrorCodeWire::ContinuationMismatch,
            detail: Some(QueryErrorDetailWire::ContinuationMismatch(
                continuation_mismatch_wire(mismatch),
            )),
            message: "continuation does not match current state".to_owned(),
        },
        other => QueryErrorWire {
            code: QueryErrorCodeWire::InvalidDeliveryRequest,
            detail: None,
            message: other.to_string(),
        },
    }
}

/// A daemon/adapter-internal failure never safe to detail on the wire.
pub fn daemon_internal(operation: &str, error: &dyn std::error::Error) -> QueryErrorWire {
    log_internal(operation, error);
    QueryErrorWire {
        code: QueryErrorCodeWire::DaemonInternal,
        detail: None,
        message: "internal daemon error; see daemon logs for detail".to_owned(),
    }
}

pub fn result_too_large(operation: &str, encoded_bytes: usize, max_bytes: usize) -> QueryErrorWire {
    QueryErrorWire {
        code: QueryErrorCodeWire::ResultTooLarge,
        detail: Some(QueryErrorDetailWire::ResultTooLarge(
            ResultTooLargeDetailWire {
                encoded_bytes,
                max_bytes,
                operation: operation.to_owned(),
            },
        )),
        message: format!(
            "the {operation} result ({encoded_bytes} bytes) exceeds the {max_bytes}-byte frame limit"
        ),
    }
}
