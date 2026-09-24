//! `brainprint.context` (#25 "brainprint.context"): change / resume /
//! rules / work_items / lineage / handoffs / structure / status. The
//! grouping is presentation/schema economy only -- each mode maps
//! straight onto its own Task 10/11 operation, never merged or
//! reinterpreted.
//!
//! `ContextParams` is a flat struct (see `find.rs`'s doc comment for
//! why: MCP requires a tool's root `inputSchema` to be `type: object`,
//! which a `#[serde(tag = "mode")]` enum's `schemars` output does not
//! provide).

use brainprint_core::protocol::query::{
    ContextWire, GroupingSpecWire, KnowledgeWire, LineageTargetWire, PathGroupRuleWire,
    ProjectionKnowledgeRefsWire, QueryOperationWire, StructureWire, SummaryResourceScopeWire,
    WorkItemStatusWire,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{
    params::{
        CorrelationParams, DeliveryParams, ParamError, ResourceKindParam, ResourceLanguageParam,
        ResourceRoleParam, TargetParam, WorkspaceSelectorParam, parse_decision_id, parse_policy_id,
        parse_work_item_id,
    },
    tools::relations::{ChangeKindParam, RelationKindParam},
};

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextMode {
    Change,
    Resume,
    Rules,
    WorkItems,
    Lineage,
    Handoffs,
    Structure,
    Status,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemStatusParam {
    Open,
    Active,
    Blocked,
    Paused,
    Completed,
    Abandoned,
}

impl From<WorkItemStatusParam> for WorkItemStatusWire {
    fn from(value: WorkItemStatusParam) -> Self {
        match value {
            WorkItemStatusParam::Open => Self::Open,
            WorkItemStatusParam::Active => Self::Active,
            WorkItemStatusParam::Blocked => Self::Blocked,
            WorkItemStatusParam::Paused => Self::Paused,
            WorkItemStatusParam::Completed => Self::Completed,
            WorkItemStatusParam::Abandoned => Self::Abandoned,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LineageOfParam {
    ProjectPolicy,
    UserPolicy,
    Decision,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PathGroupRuleParam {
    pub label: String,
    pub prefix: String,
}

/// Mirrors `GroupingSpecWire`. Nested inside `ContextParams`, not the
/// tool's root schema, so its own `#[serde(tag = ...)]` `oneOf` shape is
/// fine.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum GroupingParam {
    PathPrefixes {
        rules: Vec<PathGroupRuleParam>,
    },
    DirectoryDepth {
        root: String,
        depth: usize,
    },
    #[default]
    ResourceRole,
    ResourceLanguage,
    ResourceKind,
}

impl From<GroupingParam> for GroupingSpecWire {
    fn from(value: GroupingParam) -> Self {
        match value {
            GroupingParam::PathPrefixes { rules } => Self::PathPrefixes(
                rules
                    .into_iter()
                    .map(|rule| PathGroupRuleWire {
                        label: rule.label,
                        prefix: rule.prefix,
                    })
                    .collect(),
            ),
            GroupingParam::DirectoryDepth { root, depth } => Self::DirectoryDepth { root, depth },
            GroupingParam::ResourceRole => Self::ResourceRole,
            GroupingParam::ResourceLanguage => Self::ResourceLanguage,
            GroupingParam::ResourceKind => Self::ResourceKind,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct SummaryResourceScopeParam {
    pub path_prefix: Option<String>,
    pub role: Option<ResourceRoleParam>,
    pub language: Option<ResourceLanguageParam>,
    pub kind: Option<ResourceKindParam>,
}

impl From<SummaryResourceScopeParam> for SummaryResourceScopeWire {
    fn from(value: SummaryResourceScopeParam) -> Self {
        Self {
            path_prefix: value.path_prefix,
            role: value.role.map(Into::into),
            language: value.language.map(Into::into),
            kind: value.kind.map(Into::into),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContextParams {
    pub mode: ContextMode,

    // `change` (target required)/`resume` (target optional) fields.
    #[serde(flatten)]
    pub target: TargetParam,
    /// `mode: change` only. Omit for a plain edit with no declared
    /// change form.
    pub change: Option<ChangeKindParam>,
    /// Canonical UUID string. `mode: resume`/`handoffs`: required.
    /// `mode: change`: optional (the WorkItem this edit belongs to).
    pub work_item: Option<String>,

    /// `mode: work_items`: required.
    pub statuses: Option<Vec<WorkItemStatusParam>>,
    /// `mode: work_items`: defaults to 50. `mode: handoffs`: defaults to
    /// 20.
    pub limit: Option<usize>,

    /// `mode: lineage`: required.
    pub lineage_of: Option<LineageOfParam>,
    /// `mode: lineage`: required. Canonical UUID string of the Policy or
    /// Decision.
    pub id: Option<String>,

    // `mode: structure` fields.
    #[serde(default)]
    pub grouping: GroupingParam,
    #[serde(default)]
    pub resource_scope: SummaryResourceScopeParam,
    #[serde(default)]
    pub relation_kinds: Vec<RelationKindParam>,
    #[serde(default = "default_true")]
    pub include_ungrouped: bool,
    #[serde(default)]
    pub include_cycles: bool,
    pub member_sample_limit: Option<usize>,

    #[serde(flatten)]
    pub workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    pub correlation: CorrelationParams,
    /// `mode: change`/`resume` delivery budget. Ignored otherwise.
    #[serde(flatten)]
    pub delivery: DeliveryParams,
}

fn default_true() -> bool {
    true
}

// One per MCP tool call, never a hot loop.
#[allow(clippy::large_enum_variant)]
pub enum ContextBuild {
    /// A `Query` operation, dispatched through the shared
    /// `execute::run_query` flow.
    Query {
        workspace: WorkspaceSelectorParam,
        correlation: CorrelationParams,
        mode: &'static str,
        operation: Result<QueryOperationWire, ParamError>,
    },
    /// `context status`: the existing daemon `Status` request, no
    /// `Query`/ack step (#25 "brainprint.context").
    Status,
}

impl ContextParams {
    pub fn split(self) -> ContextBuild {
        match self.mode {
            ContextMode::Change => {
                let operation = (|| {
                    let target = self.target.into_wire()?;
                    let work_item = self
                        .work_item
                        .map(|id| parse_work_item_id(&id))
                        .transpose()?;
                    Ok(QueryOperationWire::Context(ContextWire::Change {
                        target,
                        change: self.change.map(Into::into),
                        work_item,
                        scope_layers: Vec::new(),
                        directives: Vec::new(),
                        knowledge_refs: ProjectionKnowledgeRefsWire::default(),
                        delivery: self.delivery.into_wire()?,
                    }))
                })();
                ContextBuild::Query {
                    workspace: self.workspace,
                    correlation: self.correlation,
                    mode: "change",
                    operation,
                }
            }
            ContextMode::Resume => {
                let operation = (|| {
                    let work_item = self
                        .work_item
                        .as_deref()
                        .ok_or_else(|| ParamError("mode: resume requires work_item".to_owned()))
                        .and_then(parse_work_item_id)?;
                    // #25 "brainprint.context": an absent target is a
                    // real, valid Resume with no target -- every
                    // `TargetParam` field is optional, so a zero
                    // selector count here means "no target" rather
                    // than the usual "exactly one selector required".
                    let target = if target_is_empty(&self.target) {
                        None
                    } else {
                        Some(self.target.into_wire()?)
                    };
                    Ok(QueryOperationWire::Context(ContextWire::Resume {
                        work_item,
                        target,
                        scope_layers: Vec::new(),
                        directives: Vec::new(),
                        knowledge_refs: ProjectionKnowledgeRefsWire::default(),
                        delivery: self.delivery.into_wire()?,
                    }))
                })();
                ContextBuild::Query {
                    workspace: self.workspace,
                    correlation: self.correlation,
                    mode: "resume",
                    operation,
                }
            }
            ContextMode::Rules => ContextBuild::Query {
                workspace: self.workspace,
                correlation: self.correlation,
                mode: "rules",
                operation: Ok(QueryOperationWire::Knowledge(KnowledgeWire::Rules {
                    scope_layers: Vec::new(),
                    directives: Vec::new(),
                    knowledge_refs: ProjectionKnowledgeRefsWire::default(),
                })),
            },
            ContextMode::WorkItems => {
                let operation = (|| {
                    let statuses = self.statuses.ok_or_else(|| {
                        ParamError("mode: work_items requires statuses".to_owned())
                    })?;
                    let limit = std::num::NonZeroUsize::new(self.limit.unwrap_or(50))
                        .ok_or_else(|| ParamError("limit must be greater than zero".to_owned()))?;
                    Ok(QueryOperationWire::Knowledge(KnowledgeWire::WorkItems {
                        statuses: statuses.into_iter().map(Into::into).collect(),
                        limit,
                    }))
                })();
                ContextBuild::Query {
                    workspace: self.workspace,
                    correlation: self.correlation,
                    mode: "work_items",
                    operation,
                }
            }
            ContextMode::Lineage => {
                let operation = (|| {
                    let lineage_of = self.lineage_of.ok_or_else(|| {
                        ParamError("mode: lineage requires lineage_of".to_owned())
                    })?;
                    let id = self
                        .id
                        .ok_or_else(|| ParamError("mode: lineage requires id".to_owned()))?;
                    let target = match lineage_of {
                        LineageOfParam::ProjectPolicy => {
                            LineageTargetWire::ProjectPolicy(parse_policy_id(&id)?)
                        }
                        LineageOfParam::UserPolicy => {
                            LineageTargetWire::UserPolicy(parse_policy_id(&id)?)
                        }
                        LineageOfParam::Decision => {
                            LineageTargetWire::Decision(parse_decision_id(&id)?)
                        }
                    };
                    Ok(QueryOperationWire::Knowledge(KnowledgeWire::Lineage(
                        target,
                    )))
                })();
                ContextBuild::Query {
                    workspace: self.workspace,
                    correlation: self.correlation,
                    mode: "lineage",
                    operation,
                }
            }
            ContextMode::Handoffs => {
                let operation = (|| {
                    let work_item = self
                        .work_item
                        .as_deref()
                        .ok_or_else(|| ParamError("mode: handoffs requires work_item".to_owned()))
                        .and_then(parse_work_item_id)?;
                    let limit = std::num::NonZeroUsize::new(self.limit.unwrap_or(20))
                        .ok_or_else(|| ParamError("limit must be greater than zero".to_owned()))?;
                    Ok(QueryOperationWire::Knowledge(KnowledgeWire::Handoffs {
                        work_item,
                        limit,
                    }))
                })();
                ContextBuild::Query {
                    workspace: self.workspace,
                    correlation: self.correlation,
                    mode: "handoffs",
                    operation,
                }
            }
            ContextMode::Structure => {
                let operation = Ok(QueryOperationWire::Structure(StructureWire {
                    grouping: self.grouping.into(),
                    resource_scope: self.resource_scope.into(),
                    relation_kinds: self.relation_kinds.into_iter().map(Into::into).collect(),
                    include_ungrouped: self.include_ungrouped,
                    include_cycles: self.include_cycles,
                    member_sample_limit: self
                        .member_sample_limit
                        .and_then(std::num::NonZeroUsize::new),
                }));
                ContextBuild::Query {
                    workspace: self.workspace,
                    correlation: self.correlation,
                    mode: "structure",
                    operation,
                }
            }
            ContextMode::Status => ContextBuild::Status,
        }
    }
}

fn target_is_empty(target: &TargetParam) -> bool {
    target.resource_id.is_none()
        && target.resource_path.is_none()
        && target.resource_basename.is_none()
        && target.resource_prefix.is_none()
        && target.symbol_id.is_none()
        && target.qualified_symbol_name.is_none()
        && target.symbol_name.is_none()
        && target.partial_symbol_name.is_none()
        && target.target_json.is_none()
}
