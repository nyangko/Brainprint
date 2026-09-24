//! Wire mirror of the Task 10 target/selector vocabulary (#24 §5, §17).
//!
//! Every variant here is a transport-only mirror of an existing
//! `brainprint-engine` type. No heuristic parsing exists anywhere in this
//! module: a caller states exactly which selector it means.

use serde::{Deserialize, Serialize};

use crate::{ResourceId, SymbolId};

/// Mirrors `brainprint_engine::projection::ProjectionTarget`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectionTargetWire {
    Endpoint(GraphEndpointWire),
    Resource(ResourceTargetWire),
    Symbol(SymbolTargetWire),
}

/// Mirrors `brainprint_engine::projection::ResourceTarget`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceTargetWire {
    Id(ResourceId),
    Path(String),
    Basename(String),
    PathPrefix(String),
}

/// Mirrors `brainprint_engine::projection::SymbolName`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolNameWire {
    Id(SymbolId),
    QualifiedName(String),
    Name(String),
    PartialName(String),
}

/// Mirrors `brainprint_engine::projection::SymbolTarget`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolTargetWire {
    pub name: SymbolNameWire,
    pub resource: Option<ResourceId>,
    pub kind: Option<SymbolKindWire>,
    pub language: Option<ResourceLanguageWire>,
}

/// Mirrors `brainprint_engine::graph::GraphEndpoint`, variant-for-variant,
/// including the rare `External`/`Domain`/`Logical` forms only
/// `--target-json` exposes (#24 §5, §17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphEndpointWire {
    Resource(ResourceId),
    Symbol(SymbolId),
    External(ExternalEntityWire),
    Domain(DomainEntityWire),
    Logical(crate::LogicalSymbolId),
}

/// Mirrors `brainprint_engine::graph::ExternalEntity`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalEntityWire {
    pub package_identity: String,
    pub module_path: Option<String>,
    pub symbol_name: Option<String>,
    pub qualified_name: Option<String>,
    pub kind: String,
    pub resolved_version: Option<String>,
    pub declaration_locator: Option<String>,
}

/// Mirrors `brainprint_engine::graph::DomainEntity`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainEntityWire {
    pub kind: String,
    pub normalized_identity: String,
    pub namespace: Option<String>,
    pub method: Option<String>,
    pub display_label: String,
}

/// Mirrors `brainprint_engine::graph::RelationKind`'s closed vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelationKindWire {
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

/// Mirrors `brainprint_engine::symbol::SymbolKind`'s closed vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolKindWire {
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

/// Mirrors `brainprint_engine::resource::ResourceLanguage`'s closed
/// vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceLanguageWire {
    Python,
    JavaScript,
    TypeScript,
    Svelte,
    CSharp,
    Rust,
}

/// Mirrors `brainprint_engine::resource::ResourceRole`'s closed vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceRoleWire {
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

/// Mirrors `brainprint_engine::resource::ResourceKind`'s closed vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceKindWire {
    File,
    Directory,
}
