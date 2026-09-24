//! `brainprint.inspect` (#25 "brainprint.inspect"): maps directly to
//! Task 11 Inspect. No modes -- the tool schema is the request itself.

use brainprint_core::protocol::query::{InspectWire, QueryOperationWire};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::params::{
    CorrelationParams, DeliveryParams, ParamError, TargetParam, WorkspaceSelectorParam,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InspectParams {
    #[serde(flatten)]
    pub target: TargetParam,
    #[serde(flatten)]
    pub workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    pub correlation: CorrelationParams,
    #[serde(flatten)]
    pub delivery: DeliveryParams,
}

pub struct BuiltInspect {
    pub workspace: WorkspaceSelectorParam,
    pub correlation: CorrelationParams,
    pub operation: Result<QueryOperationWire, ParamError>,
}

impl InspectParams {
    pub fn split(self) -> BuiltInspect {
        let operation = self.target.into_wire().and_then(|target| {
            Ok(QueryOperationWire::Inspect(InspectWire {
                target,
                delivery: self.delivery.into_wire()?,
            }))
        });
        BuiltInspect {
            workspace: self.workspace,
            correlation: self.correlation,
            operation,
        }
    }
}
