//! Wire request -> Task 10 Core type conversion (#24 §2, §5-§8, §11).
//!
//! Explicit, bijective, adapter-side only: nothing here ranks, resolves
//! ambiguity, reads source, or invents a default. A structurally invalid
//! wire value becomes a typed `INVALID_REQUEST`/`INVALID_DELIVERY_REQUEST`
//! error here rather than reaching `brainprint-engine` malformed.

use std::{num::NonZeroUsize, time::Duration};

use brainprint_core::{WorkspaceId, protocol::query::*};
use brainprint_engine::{
    boundary::{GroupingSpec, PathGroupRule, ResourceScope as SummaryResourceScope},
    graph::{DomainEntity, ExternalEntity, GraphEndpoint, RelationKind},
    impact::ImpactIntent,
    knowledge::{DirectiveTarget, KnowledgeScope, RequestDirective, ScopeKind, WorkItemStatus},
    projection::{
        ChangeKind, ProjectionCorrelation, ProjectionKnowledgeRefs, ProjectionTarget,
        ResourceTarget, SymbolName, SymbolTarget,
        planner::{ContextRetention, DeliveryBudget, DeliveryContinuation, DeliveryKey},
    },
    query_surface::{
        ContextPurpose, ContextRequest, DeliveryOptions, FindQuery, FindRequest, ImpactRequest,
        InspectRequest, KnowledgeQuery, KnowledgeRequest, LineageTarget, OwnedTextPattern,
        QueryContext, RelationDirection, RelationsRequest,
    },
    resource::{ResourceKind, ResourceLanguage, ResourceRole},
    search::SearchBudget,
    symbol::SymbolKind,
};

fn invalid(reason: InvalidRequestReasonWire, message: impl Into<String>) -> QueryErrorWire {
    QueryErrorWire {
        code: QueryErrorCodeWire::InvalidRequest,
        detail: Some(QueryErrorDetailWire::InvalidRequest(reason)),
        message: message.into(),
    }
}

fn delivery_request_error(message: impl Into<String>) -> QueryErrorWire {
    QueryErrorWire {
        code: QueryErrorCodeWire::InvalidDeliveryRequest,
        detail: None,
        message: message.into(),
    }
}

fn parse_hex32(value: &str, field: &str) -> Result<[u8; 32], QueryErrorWire> {
    if value.len() != 64 {
        return Err(invalid(
            InvalidRequestReasonWire::InvalidScopeLayer,
            format!("{field} must be 64 lowercase hex characters"),
        ));
    }
    let mut out = [0_u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).map_err(|_| {
            invalid(
                InvalidRequestReasonWire::InvalidScopeLayer,
                format!("{field} is not valid hex"),
            )
        })?;
    }
    Ok(out)
}

// ------------------------------------------------------------ target

pub fn resource_target(wire: ResourceTargetWire) -> ResourceTarget {
    match wire {
        ResourceTargetWire::Id(id) => ResourceTarget::Id(id),
        ResourceTargetWire::Path(path) => ResourceTarget::Path(path),
        ResourceTargetWire::Basename(name) => ResourceTarget::Basename(name),
        ResourceTargetWire::PathPrefix(prefix) => ResourceTarget::PathPrefix(prefix),
    }
}

pub fn symbol_name(wire: SymbolNameWire) -> SymbolName {
    match wire {
        SymbolNameWire::Id(id) => SymbolName::Id(id),
        SymbolNameWire::QualifiedName(name) => SymbolName::QualifiedName(name),
        SymbolNameWire::Name(name) => SymbolName::Name(name),
        SymbolNameWire::PartialName(name) => SymbolName::PartialName(name),
    }
}

pub fn symbol_kind(wire: SymbolKindWire) -> SymbolKind {
    match wire {
        SymbolKindWire::Class => SymbolKind::Class,
        SymbolKindWire::Interface => SymbolKind::Interface,
        SymbolKindWire::Struct => SymbolKind::Struct,
        SymbolKindWire::Enum => SymbolKind::Enum,
        SymbolKindWire::Trait => SymbolKind::Trait,
        SymbolKindWire::Record => SymbolKind::Record,
        SymbolKindWire::Function => SymbolKind::Function,
        SymbolKindWire::Method => SymbolKind::Method,
        SymbolKindWire::TypeAlias => SymbolKind::TypeAlias,
        SymbolKindWire::Field => SymbolKind::Field,
        SymbolKindWire::Property => SymbolKind::Property,
        SymbolKindWire::Constant => SymbolKind::Constant,
    }
}

pub fn resource_language(wire: ResourceLanguageWire) -> ResourceLanguage {
    match wire {
        ResourceLanguageWire::Python => ResourceLanguage::Python,
        ResourceLanguageWire::JavaScript => ResourceLanguage::JavaScript,
        ResourceLanguageWire::TypeScript => ResourceLanguage::TypeScript,
        ResourceLanguageWire::Svelte => ResourceLanguage::Svelte,
        ResourceLanguageWire::CSharp => ResourceLanguage::CSharp,
        ResourceLanguageWire::Rust => ResourceLanguage::Rust,
    }
}

pub fn resource_role(wire: ResourceRoleWire) -> ResourceRole {
    match wire {
        ResourceRoleWire::Source => ResourceRole::Source,
        ResourceRoleWire::Test => ResourceRole::Test,
        ResourceRoleWire::Config => ResourceRole::Config,
        ResourceRoleWire::Docs => ResourceRole::Docs,
        ResourceRoleWire::Generated => ResourceRole::Generated,
        ResourceRoleWire::Dependency => ResourceRole::Dependency,
        ResourceRoleWire::Asset => ResourceRole::Asset,
        ResourceRoleWire::ToolState => ResourceRole::ToolState,
        ResourceRoleWire::Unknown => ResourceRole::Unknown,
    }
}

pub fn resource_kind(wire: ResourceKindWire) -> ResourceKind {
    match wire {
        ResourceKindWire::File => ResourceKind::File,
        ResourceKindWire::Directory => ResourceKind::Directory,
    }
}

pub fn relation_kind(wire: RelationKindWire) -> RelationKind {
    match wire {
        RelationKindWire::Calls => RelationKind::Calls,
        RelationKindWire::References => RelationKind::References,
        RelationKindWire::Imports => RelationKind::Imports,
        RelationKindWire::Extends => RelationKind::Extends,
        RelationKindWire::Implements => RelationKind::Implements,
        RelationKindWire::Overrides => RelationKind::Overrides,
        RelationKindWire::UsesType => RelationKind::UsesType,
        RelationKindWire::UsesEnv => RelationKind::UsesEnv,
        RelationKindWire::UsesConfig => RelationKind::UsesConfig,
    }
}

pub fn symbol_target(wire: SymbolTargetWire) -> SymbolTarget {
    SymbolTarget {
        name: symbol_name(wire.name),
        resource: wire.resource,
        kind: wire.kind.map(symbol_kind),
        language: wire.language.map(resource_language),
    }
}

pub fn graph_endpoint(wire: GraphEndpointWire) -> Result<GraphEndpoint, QueryErrorWire> {
    Ok(match wire {
        GraphEndpointWire::Resource(id) => GraphEndpoint::Resource(id),
        GraphEndpointWire::Symbol(id) => GraphEndpoint::Symbol(id),
        GraphEndpointWire::External(entity) => GraphEndpoint::External(ExternalEntity {
            package_identity: entity.package_identity,
            module_path: entity.module_path,
            symbol_name: entity.symbol_name,
            qualified_name: entity.qualified_name,
            kind: entity.kind,
            resolved_version: entity.resolved_version,
            declaration_locator: entity.declaration_locator,
        }),
        GraphEndpointWire::Domain(entity) => GraphEndpoint::Domain(DomainEntity {
            kind: entity.kind,
            normalized_identity: entity.normalized_identity,
            namespace: entity.namespace,
            method: entity.method,
            display_label: entity.display_label,
        }),
        GraphEndpointWire::Logical(id) => GraphEndpoint::Logical(id),
    })
}

pub fn projection_target(wire: ProjectionTargetWire) -> Result<ProjectionTarget, QueryErrorWire> {
    Ok(match wire {
        ProjectionTargetWire::Endpoint(endpoint) => {
            ProjectionTarget::Endpoint(graph_endpoint(endpoint)?)
        }
        ProjectionTargetWire::Resource(target) => {
            ProjectionTarget::Resource(resource_target(target))
        }
        ProjectionTargetWire::Symbol(target) => ProjectionTarget::Symbol(symbol_target(target)),
    })
}

// ---------------------------------------------------------- impact/change

pub fn impact_intent(wire: ImpactIntentWire) -> ImpactIntent {
    match wire {
        ImpactIntentWire::PublicSignatureChange => ImpactIntent::PublicSignatureChange,
        ImpactIntentWire::Rename => ImpactIntent::Rename,
        ImpactIntentWire::ModuleMove => ImpactIntent::ModuleMove,
        ImpactIntentWire::BaseInterfaceChange => ImpactIntent::BaseInterfaceChange,
    }
}

pub fn change_kind(wire: ChangeKindWire) -> ChangeKind {
    match wire {
        ChangeKindWire::Structural(intent) => ChangeKind::Structural(impact_intent(intent)),
        ChangeKindWire::Delete => ChangeKind::Delete,
        ChangeKindWire::DomainContractChange => ChangeKind::DomainContractChange,
    }
}

// -------------------------------------------------------------- delivery

pub fn delivery_budget(wire: DeliveryBudgetWire) -> Result<DeliveryBudget, QueryErrorWire> {
    DeliveryBudget::new(
        wire.max_items.map(NonZeroUsize::get),
        wire.max_bytes.map(NonZeroUsize::get),
        None,
    )
    .map_err(|error| delivery_request_error(error.to_string()))
}

pub fn context_retention(wire: RetentionWire) -> ContextRetention {
    match wire {
        RetentionWire::Retained => ContextRetention::RetainedContext,
        RetentionWire::Fresh => ContextRetention::FreshContext,
        RetentionWire::Disabled => ContextRetention::ReuseDisabled,
    }
}

pub fn delivery_continuation(
    wire: DeliveryContinuationWire,
) -> Result<DeliveryContinuation, QueryErrorWire> {
    let budget = DeliveryBudget::new(
        wire.budget.max_items.map(NonZeroUsize::get),
        wire.budget.max_bytes.map(NonZeroUsize::get),
        wire.budget.max_tokens.map(NonZeroUsize::get),
    )
    .map_err(|error| delivery_request_error(error.to_string()))?;
    let next = DeliveryKey::from_wire_parts(
        wire.next.tier,
        wire.next.depth,
        parse_hex32(&wire.next.identity, "continuation.next.identity")?,
    );
    Ok(DeliveryContinuation::from_wire_parts(
        wire.workspace_id,
        wire.index_incarnation_id,
        wire.workspace_revision,
        wire.generation_no,
        wire.generation_basis_revision,
        parse_hex32(
            &wire.request_fingerprint,
            "continuation.request_fingerprint",
        )?,
        parse_hex32(
            &wire.projection_fingerprint,
            "continuation.projection_fingerprint",
        )?,
        budget,
        next,
    ))
}

/// Converted delivery inputs, plus whether the operation is
/// planner-backed (i.e. will carry a Task 8 `PendingDelivery` the daemon
/// must issue an `ack_token` for).
pub struct ConvertedDelivery {
    pub budget: DeliveryBudget,
    pub continuation: Option<DeliveryContinuation>,
    pub retention: ContextRetention,
}

pub fn delivery_options(wire: DeliveryWire) -> Result<ConvertedDelivery, QueryErrorWire> {
    Ok(ConvertedDelivery {
        budget: delivery_budget(wire.budget)?,
        continuation: wire.continuation.map(delivery_continuation).transpose()?,
        retention: context_retention(wire.retention),
    })
}

/// [`DeliveryOptions`] borrows its token counter; Task 11 never supplies
/// one (#24 §6), so every planner-backed call passes `None` here.
pub fn as_delivery_options(converted: &ConvertedDelivery) -> DeliveryOptions<'static> {
    DeliveryOptions {
        budget: converted.budget,
        continuation: converted.continuation.clone(),
        retention: converted.retention,
        tokens: None,
    }
}

// -------------------------------------------------------------- search

pub fn search_budget(wire: SearchBudgetWire) -> SearchBudget {
    SearchBudget {
        max_results: wire.max_results,
        max_files: wire.max_files,
        max_bytes: wire.max_bytes,
        deadline: wire.deadline_ms.map(Duration::from_millis),
    }
}

pub fn text_pattern(wire: TextPatternWire) -> OwnedTextPattern {
    match wire {
        TextPatternWire::Literal(text) => OwnedTextPattern::Literal(text),
        TextPatternWire::Regex(text) => OwnedTextPattern::Regex(text),
    }
}

// ------------------------------------------------------------ knowledge

pub fn scope_kind(wire: ScopeKindWire) -> ScopeKind {
    match wire {
        ScopeKindWire::Global => ScopeKind::Global,
        ScopeKindWire::Project => ScopeKind::Project,
        ScopeKindWire::Workspace => ScopeKind::Workspace,
        ScopeKindWire::Package => ScopeKind::Package,
        ScopeKindWire::Module => ScopeKind::Module,
        ScopeKindWire::Directory => ScopeKind::Directory,
        ScopeKindWire::Resource => ScopeKind::Resource,
        ScopeKindWire::Domain => ScopeKind::Domain,
        ScopeKindWire::Task => ScopeKind::Task,
    }
}

pub fn knowledge_scope(wire: KnowledgeScopeWire) -> Result<KnowledgeScope, QueryErrorWire> {
    match (wire.kind, wire.key) {
        (ScopeKindWire::Global, None) => Ok(KnowledgeScope::global()),
        (ScopeKindWire::Project, None) => Ok(KnowledgeScope::project()),
        (ScopeKindWire::Global | ScopeKindWire::Project, Some(_)) => Err(invalid(
            InvalidRequestReasonWire::InvalidScopeLayer,
            "GLOBAL/PROJECT scope must not carry a key",
        )),
        (kind, Some(key)) => KnowledgeScope::keyed(scope_kind(kind), key).map_err(|error| {
            invalid(
                InvalidRequestReasonWire::InvalidScopeLayer,
                error.to_string(),
            )
        }),
        (_, None) => Err(invalid(
            InvalidRequestReasonWire::InvalidScopeLayer,
            "this scope kind requires a key",
        )),
    }
}

pub fn scope_layers(
    wire: Vec<Vec<KnowledgeScopeWire>>,
) -> Result<Vec<Vec<KnowledgeScope>>, QueryErrorWire> {
    wire.into_iter()
        .map(|layer| layer.into_iter().map(knowledge_scope).collect())
        .collect()
}

pub fn directive_target(wire: DirectiveTargetWire) -> DirectiveTarget {
    match wire {
        DirectiveTargetWire::Policy => DirectiveTarget::Policy,
        DirectiveTargetWire::Decision => DirectiveTarget::Decision,
        DirectiveTargetWire::Preference => DirectiveTarget::Preference,
    }
}

pub fn request_directive(wire: RequestDirectiveWire) -> Result<RequestDirective, QueryErrorWire> {
    Ok(RequestDirective {
        id: wire.id,
        target: directive_target(wire.target),
        subject_key: wire.subject_key,
        scope: knowledge_scope(wire.scope)?,
        summary: wire.summary,
    })
}

pub fn directives(
    wire: Vec<RequestDirectiveWire>,
) -> Result<Vec<RequestDirective>, QueryErrorWire> {
    wire.into_iter().map(request_directive).collect()
}

pub fn knowledge_refs(wire: ProjectionKnowledgeRefsWire) -> ProjectionKnowledgeRefs {
    ProjectionKnowledgeRefs {
        decision_topics: wire.decision_topics.into_iter().collect(),
        preference_keys: wire.preference_keys.into_iter().collect(),
        state_keys: wire.state_keys.into_iter().collect(),
        blueprint_applications: wire.blueprint_applications.into_iter().collect(),
    }
}

pub fn work_item_status(wire: WorkItemStatusWire) -> WorkItemStatus {
    match wire {
        WorkItemStatusWire::Open => WorkItemStatus::Open,
        WorkItemStatusWire::Active => WorkItemStatus::Active,
        WorkItemStatusWire::Blocked => WorkItemStatus::Blocked,
        WorkItemStatusWire::Paused => WorkItemStatus::Paused,
        WorkItemStatusWire::Completed => WorkItemStatus::Completed,
        WorkItemStatusWire::Abandoned => WorkItemStatus::Abandoned,
    }
}

pub fn lineage_target(wire: LineageTargetWire) -> LineageTarget {
    match wire {
        LineageTargetWire::ProjectPolicy(id) => LineageTarget::ProjectPolicy(id),
        LineageTargetWire::UserPolicy(id) => LineageTarget::UserPolicy(id),
        LineageTargetWire::Decision(id) => LineageTarget::Decision(id),
    }
}

// -------------------------------------------------------------- structure

pub fn path_group_rule(wire: PathGroupRuleWire) -> PathGroupRule {
    PathGroupRule {
        label: wire.label,
        prefix: wire.prefix,
    }
}

pub fn grouping_spec(wire: GroupingSpecWire) -> GroupingSpec {
    match wire {
        GroupingSpecWire::PathPrefixes(rules) => {
            GroupingSpec::PathPrefixes(rules.into_iter().map(path_group_rule).collect())
        }
        GroupingSpecWire::DirectoryDepth { root, depth } => {
            GroupingSpec::DirectoryDepth { root, depth }
        }
        GroupingSpecWire::ResourceRole => GroupingSpec::ResourceRole,
        GroupingSpecWire::ResourceLanguage => GroupingSpec::ResourceLanguage,
        GroupingSpecWire::ResourceKind => GroupingSpec::ResourceKind,
    }
}

pub fn summary_resource_scope(wire: SummaryResourceScopeWire) -> SummaryResourceScope {
    SummaryResourceScope {
        path_prefix: wire.path_prefix,
        role: wire.role.map(resource_role),
        language: wire.language.map(resource_language),
        kind: wire.kind.map(resource_kind),
    }
}

// -------------------------------------------------------- correlation

pub fn correlation(wire: CorrelationWire) -> ProjectionCorrelation {
    ProjectionCorrelation {
        client_id: wire.client_id,
        session_id: wire.session_id,
        external_task_id: wire.external_task_id,
        external_subtask_id: wire.external_subtask_id,
        role_hint: wire.role_hint,
        team_hint: wire.team_hint,
        persona_traits: wire.persona_traits.into_iter().collect(),
    }
}

pub fn query_context(
    workspace: WorkspaceId,
    correlation_wire: Option<CorrelationWire>,
) -> QueryContext {
    QueryContext {
        workspace,
        correlation: correlation_wire.map(correlation),
    }
}

// ----------------------------------------------------------- operations

/// The engine-typed form of a `QueryOperationWire`, ready for
/// `CoreQuerySurface` dispatch. Kept as an enum (rather than dispatching
/// inline) so the daemon's worker loop owns exactly one call into
/// `CoreQuerySurface` per job.
pub enum ConvertedOperation<'a> {
    Find(FindRequest<'a>),
    Inspect(InspectRequest<'a>),
    Relations(RelationsRequest),
    Impact(ImpactRequest<'a>),
    Context(ContextRequest<'a>),
    Knowledge(KnowledgeRequest),
    Structure(brainprint_engine::boundary::StructuralSummaryRequest),
}

/// Delivery inputs are converted first (by the caller) and threaded in by
/// reference, so `ConvertedOperation` never outlives the `ConvertedDelivery`
/// values it borrows through `as_delivery_options`.
#[allow(clippy::too_many_lines)]
pub fn operation<'a>(
    wire: QueryOperationWire,
    workspace: WorkspaceId,
    correlation_wire: Option<CorrelationWire>,
    deliveries: &'a mut Vec<ConvertedDelivery>,
) -> Result<ConvertedOperation<'a>, QueryErrorWire> {
    let context = query_context(workspace, correlation_wire);
    match wire {
        QueryOperationWire::Find(find) => {
            let query = match find {
                FindQueryWire::Target { target, delivery } => {
                    deliveries.push(delivery_options(delivery)?);
                    FindQuery::Target {
                        target: projection_target(target)?,
                        delivery: as_delivery_options(deliveries.last().expect("just pushed")),
                    }
                }
                FindQueryWire::Files {
                    directory,
                    recursive,
                    path_prefix,
                    role,
                    language,
                    kind,
                    limit,
                } => FindQuery::Files {
                    directory,
                    recursive,
                    path_prefix,
                    role: role.map(resource_role),
                    language: language.map(resource_language),
                    kind: kind.map(resource_kind),
                    limit,
                },
                FindQueryWire::Text {
                    pattern,
                    case_insensitive,
                    path_prefix,
                    search_budget: budget,
                    max_file_bytes,
                    with_preview,
                } => FindQuery::Text {
                    pattern: text_pattern(pattern),
                    case_insensitive,
                    path_prefix,
                    budget: search_budget(budget),
                    max_file_bytes,
                    with_preview,
                },
            };
            Ok(ConvertedOperation::Find(FindRequest { context, query }))
        }
        QueryOperationWire::Inspect(InspectWire { target, delivery }) => {
            deliveries.push(delivery_options(delivery)?);
            Ok(ConvertedOperation::Inspect(InspectRequest {
                context,
                target: projection_target(target)?,
                delivery: as_delivery_options(deliveries.last().expect("just pushed")),
            }))
        }
        QueryOperationWire::Relations(RelationsWire {
            target,
            direction,
            kinds,
        }) => Ok(ConvertedOperation::Relations(RelationsRequest {
            context,
            target: projection_target(target)?,
            direction: match direction {
                RelationDirectionWire::Outgoing => RelationDirection::Outgoing,
                RelationDirectionWire::Incoming => RelationDirection::Incoming,
                RelationDirectionWire::Both => RelationDirection::Both,
            },
            kinds: kinds.into_iter().map(relation_kind).collect(),
        })),
        QueryOperationWire::Impact(ImpactWire {
            target,
            change,
            delivery,
        }) => {
            deliveries.push(delivery_options(delivery)?);
            Ok(ConvertedOperation::Impact(ImpactRequest {
                context,
                target: projection_target(target)?,
                change: change_kind(change),
                delivery: as_delivery_options(deliveries.last().expect("just pushed")),
            }))
        }
        QueryOperationWire::Context(purpose) => {
            let (purpose, scope_layers_wire, directives_wire, refs_wire, delivery_wire) =
                match purpose {
                    ContextWire::Change {
                        target,
                        change,
                        work_item,
                        scope_layers,
                        directives,
                        knowledge_refs,
                        delivery,
                    } => (
                        ContextPurpose::Change {
                            target: projection_target(target)?,
                            change: change.map(change_kind),
                            work_item,
                        },
                        scope_layers,
                        directives,
                        knowledge_refs,
                        delivery,
                    ),
                    ContextWire::Resume {
                        work_item,
                        target,
                        scope_layers,
                        directives,
                        knowledge_refs,
                        delivery,
                    } => (
                        ContextPurpose::Resume {
                            work_item,
                            target: target.map(projection_target).transpose()?,
                        },
                        scope_layers,
                        directives,
                        knowledge_refs,
                        delivery,
                    ),
                };
            deliveries.push(delivery_options(delivery_wire)?);
            Ok(ConvertedOperation::Context(ContextRequest {
                context,
                purpose,
                scope_layers: self::scope_layers(scope_layers_wire)?,
                directives: self::directives(directives_wire)?,
                knowledge: knowledge_refs(refs_wire),
                delivery: as_delivery_options(deliveries.last().expect("just pushed")),
            }))
        }
        QueryOperationWire::Knowledge(knowledge) => {
            let query = match knowledge {
                KnowledgeWire::Rules {
                    scope_layers: layers,
                    directives: dirs,
                    knowledge_refs: refs,
                } => KnowledgeQuery::Rules {
                    scope_layers: self::scope_layers(layers)?,
                    directives: self::directives(dirs)?,
                    knowledge: knowledge_refs(refs),
                },
                KnowledgeWire::WorkItems { statuses, limit } => KnowledgeQuery::WorkItems {
                    statuses: statuses.into_iter().map(work_item_status).collect(),
                    limit,
                },
                KnowledgeWire::Lineage(target) => KnowledgeQuery::Lineage(lineage_target(target)),
                KnowledgeWire::Handoffs { work_item, limit } => {
                    KnowledgeQuery::Handoffs { work_item, limit }
                }
            };
            Ok(ConvertedOperation::Knowledge(KnowledgeRequest {
                context,
                query,
            }))
        }
        QueryOperationWire::Structure(structure) => Ok(ConvertedOperation::Structure(
            brainprint_engine::boundary::StructuralSummaryRequest {
                workspace,
                grouping: grouping_spec(structure.grouping),
                resource_scope: summary_resource_scope(structure.resource_scope),
                relation_kinds: structure
                    .relation_kinds
                    .into_iter()
                    .map(relation_kind)
                    .collect(),
                include_ungrouped: structure.include_ungrouped,
                include_cycles: structure.include_cycles,
                member_sample_limit: structure.member_sample_limit,
            },
        )),
    }
}
