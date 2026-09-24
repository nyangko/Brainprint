//! `brainprint.find` (#25 "brainprint.find"): target / files / explicit
//! text. Text never runs automatically after a structured miss --
//! callers choose the mode explicitly.
//!
//! `FindParams` is a flat struct (a `mode` selector plus every mode's
//! fields as siblings) rather than a `#[serde(tag = "mode")]` enum: MCP
//! requires a tool's root `inputSchema` to declare `"type": "object"`,
//! which `schemars`' internally-tagged-enum schema (`oneOf` at the root,
//! no root `type`) does not produce -- `rmcp` refuses to register such a
//! tool at all. `split()` still enforces exactly-one-mode's-worth of
//! fields at request time, same as before.

use brainprint_core::protocol::query::{
    FindQueryWire, ProjectionTargetWire, QueryOperationWire, TextPatternWire,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::params::{
    CorrelationParams, DeliveryParams, ParamError, ResourceKindParam, ResourceLanguageParam,
    ResourceRoleParam, SearchBudgetProfileParam, TargetParam, WorkspaceSelectorParam,
};

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FindMode {
    Target,
    Files,
    Text,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindParams {
    pub mode: FindMode,

    /// `mode: target` selector. Ignored for `files`/`text`.
    #[serde(flatten)]
    pub target: TargetParam,

    // `mode: files` fields.
    pub directory: Option<String>,
    #[serde(default)]
    pub recursive: bool,
    /// `files`: a Resource path prefix filter. `text`: a search scope
    /// prefix. Same field, same meaning, whichever mode reads it.
    pub path_prefix: Option<String>,
    pub role: Option<ResourceRoleParam>,
    pub language: Option<ResourceLanguageParam>,
    pub kind: Option<ResourceKindParam>,
    /// `files`: defaults to 100. Ignored for other modes.
    pub limit: Option<usize>,

    // `mode: text` fields.
    pub pattern: Option<String>,
    /// `pattern` is a regular expression rather than a literal string
    /// when `true`.
    #[serde(default)]
    pub regex: bool,
    #[serde(default)]
    pub case_insensitive: bool,
    #[serde(default = "default_true")]
    pub with_preview: bool,
    #[serde(default)]
    pub search_budget_profile: SearchBudgetProfileParam,

    #[serde(flatten)]
    pub workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    pub correlation: CorrelationParams,
    /// `mode: target` delivery budget. Ignored for `files`/`text`.
    #[serde(flatten)]
    pub delivery: DeliveryParams,
}

fn default_true() -> bool {
    true
}

pub struct BuiltFind {
    pub workspace: WorkspaceSelectorParam,
    pub correlation: CorrelationParams,
    pub mode: &'static str,
    pub operation: Result<QueryOperationWire, ParamError>,
}

impl FindParams {
    pub fn split(self) -> BuiltFind {
        match self.mode {
            FindMode::Target => {
                let operation = self
                    .target
                    .into_wire()
                    .and_then(|target: ProjectionTargetWire| {
                        Ok(QueryOperationWire::Find(FindQueryWire::Target {
                            target,
                            delivery: self.delivery.into_wire()?,
                        }))
                    });
                BuiltFind {
                    workspace: self.workspace,
                    correlation: self.correlation,
                    mode: "target",
                    operation,
                }
            }
            FindMode::Files => {
                let operation = std::num::NonZeroUsize::new(self.limit.unwrap_or(100))
                    .ok_or_else(|| ParamError("limit must be greater than zero".to_owned()))
                    .map(|limit| {
                        QueryOperationWire::Find(FindQueryWire::Files {
                            directory: self.directory,
                            recursive: self.recursive,
                            path_prefix: self.path_prefix,
                            role: self.role.map(Into::into),
                            language: self.language.map(Into::into),
                            kind: self.kind.map(Into::into),
                            limit,
                        })
                    });
                BuiltFind {
                    workspace: self.workspace,
                    correlation: self.correlation,
                    mode: "files",
                    operation,
                }
            }
            FindMode::Text => {
                let operation = self
                    .pattern
                    .ok_or_else(|| ParamError("mode: text requires pattern".to_owned()))
                    .map(|pattern| {
                        let (search_budget, max_file_bytes) =
                            self.search_budget_profile.into_wire();
                        let pattern = if self.regex {
                            TextPatternWire::Regex(pattern)
                        } else {
                            TextPatternWire::Literal(pattern)
                        };
                        QueryOperationWire::Find(FindQueryWire::Text {
                            pattern,
                            case_insensitive: self.case_insensitive,
                            path_prefix: self.path_prefix,
                            search_budget,
                            max_file_bytes,
                            with_preview: self.with_preview,
                        })
                    });
                BuiltFind {
                    workspace: self.workspace,
                    correlation: self.correlation,
                    mode: "text",
                    operation,
                }
            }
        }
    }
}
