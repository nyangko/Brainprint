//! Shared MCP tool-input JSON shapes and their bijective mapping onto
//! Task 11 wire types (#25 "Tool input design"). Mirrors
//! `brainprint-cli::query::{target,delivery}`'s conversion pattern, which
//! already solves the same "adapter grammar -> wire type" problem for
//! the same wire contract -- reimplemented here as small JSON-schema
//! structs instead of `clap::Args` because the transport is JSON, not
//! flags. `brainprint-core`'s wire types are not derived with
//! `schemars::JsonSchema` (Task 11's locked contract stays untouched),
//! so identities cross this boundary as canonical UUID strings, parsed
//! with the same `FromStr` Task 11's own wire (de)serialization uses.

use std::path::Path;

use brainprint_core::{
    DecisionId, PolicyId, ResourceId, SymbolId, WorkItemId, WorkspaceId,
    protocol::query::{
        ContinuationBudgetWire, CorrelationWire, DeliveryBudgetWire, DeliveryContinuationWire,
        DeliveryKeyWire, DeliveryWire, ProjectionTargetWire, ResourceKindWire,
        ResourceLanguageWire, ResourceRoleWire, ResourceTargetWire, RetentionWire,
        SearchBudgetWire, SymbolKindWire, SymbolNameWire, SymbolTargetWire, WorkspaceSelectorWire,
    },
};
use schemars::JsonSchema;
use serde::Deserialize;

/// A field this adapter could not turn into a valid Task 11 wire value --
/// always a client-visible `invalid_params` protocol error (#25 "Error
/// mapping" layer 1: the request itself is unroutable), never something
/// silently guessed.
#[derive(Debug)]
pub struct ParamError(pub String);

impl ParamError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

// ------------------------------------------------------------ workspace

/// #25 "Workspace": exactly one logical selector after normalization.
/// Never first-match, never auto-init/sync/repair -- this only shapes
/// the request; `WORKSPACE_AMBIGUOUS`/`NOT_INITIALIZED`/etc. remain the
/// daemon's typed answers.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct WorkspaceSelectorParam {
    /// Absolute path to the Workspace root. Falls back to
    /// `brainprint-mcp`'s startup working directory when neither this
    /// nor `workspace_id` is given.
    pub workspace_path: Option<String>,
    /// An explicit Workspace identity (canonical UUID string), for a
    /// caller that already resolved one.
    pub workspace_id: Option<String>,
}

impl WorkspaceSelectorParam {
    pub fn into_wire(self, startup_cwd: &Path) -> Result<WorkspaceSelectorWire, ParamError> {
        match (self.workspace_path, self.workspace_id) {
            (Some(_), Some(_)) => Err(ParamError::new(
                "workspace_path and workspace_id are mutually exclusive; supply exactly one",
            )),
            (Some(path), None) => Ok(WorkspaceSelectorWire::Locator {
                path: absolute_workspace_path(Path::new(&path)),
            }),
            (None, Some(id)) => id
                .parse::<WorkspaceId>()
                .map(|workspace_id| WorkspaceSelectorWire::Id { workspace_id })
                .map_err(|_| ParamError::new("workspace_id is not a valid UUID")),
            (None, None) => Ok(WorkspaceSelectorWire::Locator {
                path: absolute_workspace_path(startup_cwd),
            }),
        }
    }
}

/// #25 "Workspace selection": "CLI input path / current working
/// directory -> normalize/canonicalize in the client -> daemon
/// `resolve_workspace`". `resolve_workspace`/`find_by_locator` compare
/// the locator as opaque text (no canonicalization at the daemon layer),
/// exactly mirroring `brainprint-cli::query::exec::absolute_workspace` --
/// the client, not the daemon, is responsible for this.
fn absolute_workspace_path(path: &Path) -> String {
    path.canonicalize()
        .or_else(|_| std::env::current_dir().map(|cwd| cwd.join(path)))
        .map(|absolute| absolute.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

// ----------------------------------------------------------- correlation

/// #25 "Correlation": diagnostic/correlation only, never authorization.
/// Role/team/persona are deliberately absent from the MCP schema.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct CorrelationParams {
    pub client_id: Option<String>,
    pub session_id: Option<String>,
    pub external_task_id: Option<String>,
    pub external_subtask_id: Option<String>,
}

impl CorrelationParams {
    pub fn into_wire(self) -> Option<CorrelationWire> {
        if self.client_id.is_none()
            && self.session_id.is_none()
            && self.external_task_id.is_none()
            && self.external_subtask_id.is_none()
        {
            return None;
        }
        Some(CorrelationWire {
            client_id: self.client_id,
            session_id: self.session_id,
            external_task_id: self.external_task_id,
            external_subtask_id: self.external_subtask_id,
            // #25 "Correlation": not authority, not part of the P0 MCP
            // schema.
            role_hint: None,
            team_hint: None,
            persona_traits: Vec::new(),
        })
    }
}

// --------------------------------------------------------------- budget

/// #25 "Budget contract"'s exact locked profiles.
#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BudgetProfileParam {
    #[default]
    Compact,
    Standard,
    Wide,
}

impl BudgetProfileParam {
    const fn items_and_bytes(self) -> (usize, usize) {
        const KIB: usize = 1024;
        match self {
            Self::Compact => (16, 16 * KIB),
            Self::Standard => (64, 64 * KIB),
            Self::Wide => (128, 128 * KIB),
        }
    }
}

/// Delivery inputs for a planner-backed mode (find/target, inspect,
/// relations/impact, context/change, context/resume). `retention` is
/// deliberately absent here: #25 P0 safe default is always
/// `ReuseDisabled` (see `crate::retention`).
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(default)]
pub struct DeliveryParams {
    pub budget_profile: BudgetProfileParam,
    /// Overrides the profile's `max_items` axis.
    pub max_items: Option<usize>,
    /// Overrides the profile's `max_bytes` axis.
    pub max_bytes: Option<usize>,
    /// The exact `DeliveryContinuationWire` a previous call returned
    /// under `more_available: true`. Opaque -- echo it back verbatim,
    /// never hand-construct it.
    pub continuation: Option<DeliveryContinuationParam>,
}

impl DeliveryParams {
    pub fn into_wire(self) -> Result<DeliveryWire, ParamError> {
        let (profile_items, profile_bytes) = self.budget_profile.items_and_bytes();
        let max_items = self.max_items.unwrap_or(profile_items);
        let max_bytes = self.max_bytes.unwrap_or(profile_bytes);
        let max_items = std::num::NonZeroUsize::new(max_items)
            .ok_or_else(|| ParamError::new("max_items must be greater than zero"))?;
        let max_bytes = std::num::NonZeroUsize::new(max_bytes)
            .ok_or_else(|| ParamError::new("max_bytes must be greater than zero"))?;
        Ok(DeliveryWire {
            budget: DeliveryBudgetWire {
                max_items: Some(max_items),
                max_bytes: Some(max_bytes),
            },
            continuation: self
                .continuation
                .map(DeliveryContinuationParam::into_wire)
                .transpose()?,
            // #25 "Retention / acknowledgement": P0 safe default, always.
            retention: RetentionWire::Disabled,
        })
    }
}

/// Field-for-field mirror of `DeliveryContinuationWire`, so it carries a
/// real JSON Schema across the MCP boundary without deriving
/// `schemars::JsonSchema` on the locked Task 11 wire type itself. Every
/// field round-trips losslessly through `into_wire`/`from_wire`.
#[derive(Debug, Clone, Deserialize, JsonSchema, serde::Serialize)]
pub struct DeliveryContinuationParam {
    pub workspace_id: String,
    pub index_incarnation_id: String,
    pub workspace_revision: String,
    pub generation_no: i64,
    pub generation_basis_revision: String,
    pub request_fingerprint: String,
    pub projection_fingerprint: String,
    pub budget: ContinuationBudgetParam,
    pub next: DeliveryKeyParam,
}

#[derive(Debug, Clone, Deserialize, JsonSchema, serde::Serialize)]
pub struct ContinuationBudgetParam {
    pub max_items: Option<usize>,
    pub max_bytes: Option<usize>,
    pub max_tokens: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema, serde::Serialize)]
pub struct DeliveryKeyParam {
    pub tier: u8,
    pub depth: usize,
    pub identity: String,
}

impl DeliveryContinuationParam {
    /// Every field round-trips losslessly (#25 "Continuation": "Use
    /// `DeliveryContinuationWire` directly... no MCP-side cursor/session
    /// map"). A malformed identity is a real `invalid_params` failure,
    /// never silently coerced into a wrong-but-valid value that would
    /// surface as a confusing daemon-side `CONTINUATION_MISMATCH`
    /// instead of a clear adapter-side rejection.
    fn into_wire(self) -> Result<DeliveryContinuationWire, ParamError> {
        Ok(DeliveryContinuationWire {
            workspace_id: parse_id(&self.workspace_id, "continuation.workspace_id")?,
            index_incarnation_id: parse_id(
                &self.index_incarnation_id,
                "continuation.index_incarnation_id",
            )?,
            workspace_revision: self.workspace_revision,
            generation_no: self.generation_no,
            generation_basis_revision: self.generation_basis_revision,
            request_fingerprint: self.request_fingerprint,
            projection_fingerprint: self.projection_fingerprint,
            budget: ContinuationBudgetWire {
                max_items: self.budget.max_items.and_then(std::num::NonZeroUsize::new),
                max_bytes: self.budget.max_bytes.and_then(std::num::NonZeroUsize::new),
                max_tokens: self.budget.max_tokens.and_then(std::num::NonZeroUsize::new),
            },
            next: DeliveryKeyWire {
                tier: self.next.tier,
                depth: self.next.depth,
                identity: self.next.identity,
            },
        })
    }
}

/// #25 "Text SearchBudget"'s exact locked profiles -- the same numeric
/// values `brainprint-cli`'s `--search-budget` already locks (Task 11).
#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchBudgetProfileParam {
    #[default]
    Compact,
    Standard,
    Wide,
}

impl SearchBudgetProfileParam {
    #[must_use]
    pub fn into_wire(self) -> (SearchBudgetWire, u64) {
        const MIB: u64 = 1024 * 1024;
        match self {
            Self::Compact => (
                SearchBudgetWire {
                    max_results: 50,
                    max_files: 500,
                    max_bytes: 8 * MIB,
                    deadline_ms: Some(1_000),
                },
                MIB,
            ),
            Self::Standard => (
                SearchBudgetWire {
                    max_results: 200,
                    max_files: 5_000,
                    max_bytes: 64 * MIB,
                    deadline_ms: Some(3_000),
                },
                4 * MIB,
            ),
            Self::Wide => (
                SearchBudgetWire {
                    max_results: 500,
                    max_files: 20_000,
                    max_bytes: 256 * MIB,
                    deadline_ms: Some(10_000),
                },
                8 * MIB,
            ),
        }
    }
}

// ---------------------------------------------------------------- target

/// #25 "brainprint.find" / "Use existing explicit target vocabulary.
/// Never infer target type from string shape." Exactly one selector
/// field, mirroring `brainprint-cli::query::target::TargetArgs`'s
/// "exactly one" rule and its `target_json` escape hatch for the rare
/// `GraphEndpoint::{External,Domain,Logical}` forms.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct TargetParam {
    pub resource_id: Option<String>,
    pub resource_path: Option<String>,
    pub resource_basename: Option<String>,
    pub resource_prefix: Option<String>,
    pub symbol_id: Option<String>,
    pub qualified_symbol_name: Option<String>,
    pub symbol_name: Option<String>,
    pub partial_symbol_name: Option<String>,
    /// Restricts a Symbol selector to one Resource (canonical UUID
    /// string). Valid only alongside a Symbol selector.
    pub symbol_in_resource: Option<String>,
    pub symbol_kind: Option<SymbolKindParam>,
    pub symbol_language: Option<ResourceLanguageParam>,
    /// The complete `ProjectionTargetWire` as JSON -- the only way to
    /// reach a rare `GraphEndpoint` form (External/Domain/Logical).
    /// Mutually exclusive with every field above.
    pub target_json: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "PascalCase")]
pub enum SymbolKindParam {
    Class,
    Interface,
    Struct,
    Enum,
    Trait,
    Record,
    Function,
    Method,
    TypeAlias,
    Field,
    Property,
    Constant,
}

impl From<SymbolKindParam> for SymbolKindWire {
    fn from(value: SymbolKindParam) -> Self {
        match value {
            SymbolKindParam::Class => Self::Class,
            SymbolKindParam::Interface => Self::Interface,
            SymbolKindParam::Struct => Self::Struct,
            SymbolKindParam::Enum => Self::Enum,
            SymbolKindParam::Trait => Self::Trait,
            SymbolKindParam::Record => Self::Record,
            SymbolKindParam::Function => Self::Function,
            SymbolKindParam::Method => Self::Method,
            SymbolKindParam::TypeAlias => Self::TypeAlias,
            SymbolKindParam::Field => Self::Field,
            SymbolKindParam::Property => Self::Property,
            SymbolKindParam::Constant => Self::Constant,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "PascalCase")]
pub enum ResourceLanguageParam {
    Python,
    JavaScript,
    TypeScript,
    Svelte,
    CSharp,
    Rust,
}

impl From<ResourceLanguageParam> for ResourceLanguageWire {
    fn from(value: ResourceLanguageParam) -> Self {
        match value {
            ResourceLanguageParam::Python => Self::Python,
            ResourceLanguageParam::JavaScript => Self::JavaScript,
            ResourceLanguageParam::TypeScript => Self::TypeScript,
            ResourceLanguageParam::Svelte => Self::Svelte,
            ResourceLanguageParam::CSharp => Self::CSharp,
            ResourceLanguageParam::Rust => Self::Rust,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "PascalCase")]
pub enum ResourceRoleParam {
    Source,
    Test,
    Config,
    Docs,
    Generated,
    Dependency,
    Asset,
    ToolState,
    Unknown,
}

impl From<ResourceRoleParam> for ResourceRoleWire {
    fn from(value: ResourceRoleParam) -> Self {
        match value {
            ResourceRoleParam::Source => Self::Source,
            ResourceRoleParam::Test => Self::Test,
            ResourceRoleParam::Config => Self::Config,
            ResourceRoleParam::Docs => Self::Docs,
            ResourceRoleParam::Generated => Self::Generated,
            ResourceRoleParam::Dependency => Self::Dependency,
            ResourceRoleParam::Asset => Self::Asset,
            ResourceRoleParam::ToolState => Self::ToolState,
            ResourceRoleParam::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "PascalCase")]
pub enum ResourceKindParam {
    File,
    Directory,
}

impl From<ResourceKindParam> for ResourceKindWire {
    fn from(value: ResourceKindParam) -> Self {
        match value {
            ResourceKindParam::File => Self::File,
            ResourceKindParam::Directory => Self::Directory,
        }
    }
}

fn parse_id<T: std::str::FromStr>(value: &str, field: &str) -> Result<T, ParamError> {
    value
        .parse()
        .map_err(|_| ParamError::new(format!("{field} is not a valid UUID")))
}

impl TargetParam {
    pub fn into_wire(self) -> Result<ProjectionTargetWire, ParamError> {
        if let Some(json) = self.target_json {
            let has_other = self.resource_id.is_some()
                || self.resource_path.is_some()
                || self.resource_basename.is_some()
                || self.resource_prefix.is_some()
                || self.symbol_id.is_some()
                || self.qualified_symbol_name.is_some()
                || self.symbol_name.is_some()
                || self.partial_symbol_name.is_some();
            if has_other {
                return Err(ParamError::new(
                    "target_json is mutually exclusive with every other target field",
                ));
            }
            return raw_target_json(json);
        }

        let mut symbol_name = Vec::new();
        if let Some(id) = self.symbol_id {
            symbol_name.push(SymbolNameWire::Id(parse_id::<SymbolId>(&id, "symbol_id")?));
        }
        if let Some(name) = self.qualified_symbol_name {
            symbol_name.push(SymbolNameWire::QualifiedName(name));
        }
        if let Some(name) = self.symbol_name {
            symbol_name.push(SymbolNameWire::Name(name));
        }
        if let Some(name) = self.partial_symbol_name {
            symbol_name.push(SymbolNameWire::PartialName(name));
        }

        let mut resource_selector = Vec::new();
        if let Some(id) = self.resource_id {
            resource_selector.push(ResourceTargetWire::Id(parse_id::<ResourceId>(
                &id,
                "resource_id",
            )?));
        }
        if let Some(path) = self.resource_path {
            resource_selector.push(ResourceTargetWire::Path(path));
        }
        if let Some(name) = self.resource_basename {
            resource_selector.push(ResourceTargetWire::Basename(name));
        }
        if let Some(prefix) = self.resource_prefix {
            resource_selector.push(ResourceTargetWire::PathPrefix(prefix));
        }

        let selector_count = symbol_name.len() + resource_selector.len();
        if selector_count == 0 {
            return Err(ParamError::new(
                "exactly one target selector is required (resource_id, resource_path, \
                 resource_basename, resource_prefix, symbol_id, qualified_symbol_name, \
                 symbol_name, partial_symbol_name, or target_json)",
            ));
        }
        if selector_count > 1 {
            return Err(ParamError::new("exactly one target selector may be set"));
        }

        if let Some(name) = symbol_name.into_iter().next() {
            let resource = self
                .symbol_in_resource
                .map(|id| parse_id::<ResourceId>(&id, "symbol_in_resource"))
                .transpose()?;
            return Ok(ProjectionTargetWire::Symbol(SymbolTargetWire {
                name,
                resource,
                kind: self.symbol_kind.map(Into::into),
                language: self.symbol_language.map(Into::into),
            }));
        }

        if self.symbol_in_resource.is_some()
            || self.symbol_kind.is_some()
            || self.symbol_language.is_some()
        {
            return Err(ParamError::new(
                "symbol_in_resource/symbol_kind/symbol_language require a Symbol selector",
            ));
        }

        Ok(ProjectionTargetWire::Resource(
            resource_selector
                .into_iter()
                .next()
                .expect("selector_count == 1"),
        ))
    }
}

fn raw_target_json(json: serde_json::Value) -> Result<ProjectionTargetWire, ParamError> {
    serde_json::from_value(json).map_err(|error| {
        ParamError::new(format!(
            "target_json is not a valid target selector: {error}"
        ))
    })
}

/// Parse a `WorkItemId`/`PolicyId`/`DecisionId` from its canonical UUID
/// string form, for tool params that name one directly.
pub fn parse_work_item_id(value: &str) -> Result<WorkItemId, ParamError> {
    parse_id(value, "work_item")
}

pub fn parse_policy_id(value: &str) -> Result<PolicyId, ParamError> {
    parse_id(value, "policy_id")
}

pub fn parse_decision_id(value: &str) -> Result<DecisionId, ParamError> {
    parse_id(value, "decision_id")
}
