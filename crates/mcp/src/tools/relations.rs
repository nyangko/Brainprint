//! `brainprint.relations` (#25 "brainprint.relations"): direct / impact.
//! `direct` stays one-anchor, one-hop, unpaged, no source, no MCP-side
//! pagination. `impact` uses the existing closed `ChangeKind` vocabulary
//! -- the caller states the change form; nothing here infers one.
//!
//! `RelationsParams` is a flat struct (see `find.rs`'s doc comment for
//! why: MCP requires a tool's root `inputSchema` to be `type: object`,
//! which a `#[serde(tag = "mode")]` enum's `schemars` output does not
//! provide).

use brainprint_core::protocol::query::{
    ChangeKindWire, ImpactIntentWire, ImpactWire, QueryOperationWire, RelationDirectionWire,
    RelationKindWire, RelationsWire,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::params::{
    CorrelationParams, DeliveryParams, ParamError, TargetParam, WorkspaceSelectorParam,
};

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RelationsMode {
    Direct,
    Impact,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RelationDirectionParam {
    Outgoing,
    Incoming,
    #[default]
    Both,
}

impl From<RelationDirectionParam> for RelationDirectionWire {
    fn from(value: RelationDirectionParam) -> Self {
        match value {
            RelationDirectionParam::Outgoing => Self::Outgoing,
            RelationDirectionParam::Incoming => Self::Incoming,
            RelationDirectionParam::Both => Self::Both,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RelationKindParam {
    Calls,
    References,
    Imports,
    Extends,
    Implements,
    Overrides,
    UsesType,
    UsesEnv,
    UsesConfig,
}

impl From<RelationKindParam> for RelationKindWire {
    fn from(value: RelationKindParam) -> Self {
        match value {
            RelationKindParam::Calls => Self::Calls,
            RelationKindParam::References => Self::References,
            RelationKindParam::Imports => Self::Imports,
            RelationKindParam::Extends => Self::Extends,
            RelationKindParam::Implements => Self::Implements,
            RelationKindParam::Overrides => Self::Overrides,
            RelationKindParam::UsesType => Self::UsesType,
            RelationKindParam::UsesEnv => Self::UsesEnv,
            RelationKindParam::UsesConfig => Self::UsesConfig,
        }
    }
}

/// #25 "brainprint.relations": the caller states the change form
/// explicitly -- the existing closed `ChangeKind` vocabulary, mirrored
/// field-for-field. No MCP/Agent inference of which one applies. Nested
/// inside `RelationsParams`, not the tool's root schema, so its own
/// `#[serde(tag = ...)]` `oneOf` shape is fine (#25's root-`type`
/// requirement applies only to the top-level tool input).
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChangeKindParam {
    Structural { intent: ImpactIntentParam },
    Delete,
    DomainContractChange,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImpactIntentParam {
    /// A public signature changed: everything that calls, overrides,
    /// implements, or names it as a type.
    PublicSignatureChange,
    /// A Symbol is being renamed: every written mention of the name.
    Rename,
    /// A module moved or was renamed: everything that imports it.
    ModuleMove,
    /// A base class or interface changed: the hierarchy under it, and
    /// what names it as a type.
    BaseInterfaceChange,
}

impl From<ChangeKindParam> for ChangeKindWire {
    fn from(value: ChangeKindParam) -> Self {
        match value {
            ChangeKindParam::Structural { intent } => Self::Structural(match intent {
                ImpactIntentParam::PublicSignatureChange => ImpactIntentWire::PublicSignatureChange,
                ImpactIntentParam::Rename => ImpactIntentWire::Rename,
                ImpactIntentParam::ModuleMove => ImpactIntentWire::ModuleMove,
                ImpactIntentParam::BaseInterfaceChange => ImpactIntentWire::BaseInterfaceChange,
            }),
            ChangeKindParam::Delete => Self::Delete,
            ChangeKindParam::DomainContractChange => Self::DomainContractChange,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RelationsParams {
    pub mode: RelationsMode,
    #[serde(flatten)]
    pub target: TargetParam,

    // `mode: direct` fields.
    #[serde(default)]
    pub direction: RelationDirectionParam,
    /// Empty means every kind. `mode: direct` only.
    #[serde(default)]
    pub kinds: Vec<RelationKindParam>,

    /// `mode: impact` only, required for that mode.
    pub change: Option<ChangeKindParam>,

    #[serde(flatten)]
    pub workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    pub correlation: CorrelationParams,
    /// `mode: impact` delivery budget. Ignored for `direct`.
    #[serde(flatten)]
    pub delivery: DeliveryParams,
}

pub struct BuiltRelations {
    pub workspace: WorkspaceSelectorParam,
    pub correlation: CorrelationParams,
    pub mode: &'static str,
    pub operation: Result<QueryOperationWire, ParamError>,
}

impl RelationsParams {
    pub fn split(self) -> BuiltRelations {
        match self.mode {
            RelationsMode::Direct => {
                let operation = self.target.into_wire().map(|target| {
                    QueryOperationWire::Relations(RelationsWire {
                        target,
                        direction: self.direction.into(),
                        kinds: self.kinds.into_iter().map(Into::into).collect(),
                    })
                });
                BuiltRelations {
                    workspace: self.workspace,
                    correlation: self.correlation,
                    mode: "direct",
                    operation,
                }
            }
            RelationsMode::Impact => {
                let operation = (|| {
                    let target = self.target.into_wire()?;
                    let change = self
                        .change
                        .ok_or_else(|| ParamError("mode: impact requires change".to_owned()))?;
                    Ok(QueryOperationWire::Impact(ImpactWire {
                        target,
                        change: change.into(),
                        delivery: self.delivery.into_wire()?,
                    }))
                })();
                BuiltRelations {
                    workspace: self.workspace,
                    correlation: self.correlation,
                    mode: "impact",
                    operation,
                }
            }
        }
    }
}
