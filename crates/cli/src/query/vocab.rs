//! Closed CLI vocabularies and their exact wire spellings (#24 §19).
//!
//! Each `ValueEnum` here is CLI-facing only; `clap` never becomes a
//! dependency of `brainprint-core`. No free-form fallback exists anywhere
//! in this module -- an unrecognized spelling is a `clap` parse error
//! (exit 2), never a guess.

use brainprint_core::protocol::query::*;
use clap::ValueEnum;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SymbolKindArg {
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

impl From<SymbolKindArg> for SymbolKindWire {
    fn from(value: SymbolKindArg) -> Self {
        match value {
            SymbolKindArg::Class => Self::Class,
            SymbolKindArg::Interface => Self::Interface,
            SymbolKindArg::Struct => Self::Struct,
            SymbolKindArg::Enum => Self::Enum,
            SymbolKindArg::Trait => Self::Trait,
            SymbolKindArg::Record => Self::Record,
            SymbolKindArg::Function => Self::Function,
            SymbolKindArg::Method => Self::Method,
            SymbolKindArg::TypeAlias => Self::TypeAlias,
            SymbolKindArg::Field => Self::Field,
            SymbolKindArg::Property => Self::Property,
            SymbolKindArg::Constant => Self::Constant,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum LanguageArg {
    Python,
    Javascript,
    Typescript,
    Svelte,
    Csharp,
    Rust,
}

impl From<LanguageArg> for ResourceLanguageWire {
    fn from(value: LanguageArg) -> Self {
        match value {
            LanguageArg::Python => Self::Python,
            LanguageArg::Javascript => Self::JavaScript,
            LanguageArg::Typescript => Self::TypeScript,
            LanguageArg::Svelte => Self::Svelte,
            LanguageArg::Csharp => Self::CSharp,
            LanguageArg::Rust => Self::Rust,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RoleArg {
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

impl From<RoleArg> for ResourceRoleWire {
    fn from(value: RoleArg) -> Self {
        match value {
            RoleArg::Source => Self::Source,
            RoleArg::Test => Self::Test,
            RoleArg::Config => Self::Config,
            RoleArg::Docs => Self::Docs,
            RoleArg::Generated => Self::Generated,
            RoleArg::Dependency => Self::Dependency,
            RoleArg::Asset => Self::Asset,
            RoleArg::ToolState => Self::ToolState,
            RoleArg::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ResourceKindArg {
    File,
    Directory,
}

impl From<ResourceKindArg> for ResourceKindWire {
    fn from(value: ResourceKindArg) -> Self {
        match value {
            ResourceKindArg::File => Self::File,
            ResourceKindArg::Directory => Self::Directory,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RelationKindArg {
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

impl From<RelationKindArg> for RelationKindWire {
    fn from(value: RelationKindArg) -> Self {
        match value {
            RelationKindArg::Calls => Self::Calls,
            RelationKindArg::References => Self::References,
            RelationKindArg::Imports => Self::Imports,
            RelationKindArg::Extends => Self::Extends,
            RelationKindArg::Implements => Self::Implements,
            RelationKindArg::Overrides => Self::Overrides,
            RelationKindArg::UsesType => Self::UsesType,
            RelationKindArg::UsesEnv => Self::UsesEnv,
            RelationKindArg::UsesConfig => Self::UsesConfig,
        }
    }
}

/// #24 §19's exact closed spellings for `--change`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ChangeArg {
    PublicSignature,
    Rename,
    ModuleMove,
    BaseInterface,
    Delete,
    DomainContract,
}

impl From<ChangeArg> for ChangeKindWire {
    fn from(value: ChangeArg) -> Self {
        match value {
            ChangeArg::PublicSignature => Self::Structural(ImpactIntentWire::PublicSignatureChange),
            ChangeArg::Rename => Self::Structural(ImpactIntentWire::Rename),
            ChangeArg::ModuleMove => Self::Structural(ImpactIntentWire::ModuleMove),
            ChangeArg::BaseInterface => Self::Structural(ImpactIntentWire::BaseInterfaceChange),
            ChangeArg::Delete => Self::Delete,
            ChangeArg::DomainContract => Self::DomainContractChange,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum StatusArg {
    Open,
    Active,
    Blocked,
    Paused,
    Completed,
    Abandoned,
}

impl From<StatusArg> for WorkItemStatusWire {
    fn from(value: StatusArg) -> Self {
        match value {
            StatusArg::Open => Self::Open,
            StatusArg::Active => Self::Active,
            StatusArg::Blocked => Self::Blocked,
            StatusArg::Paused => Self::Paused,
            StatusArg::Completed => Self::Completed,
            StatusArg::Abandoned => Self::Abandoned,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DirectionArg {
    Outgoing,
    Incoming,
    Both,
}

impl From<DirectionArg> for RelationDirectionWire {
    fn from(value: DirectionArg) -> Self {
        match value {
            DirectionArg::Outgoing => Self::Outgoing,
            DirectionArg::Incoming => Self::Incoming,
            DirectionArg::Both => Self::Both,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum BudgetProfileArg {
    Compact,
    Standard,
    Wide,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RetentionArg {
    Retained,
    Fresh,
    Disabled,
}

impl From<RetentionArg> for RetentionWire {
    fn from(value: RetentionArg) -> Self {
        match value {
            RetentionArg::Retained => Self::Retained,
            RetentionArg::Fresh => Self::Fresh,
            RetentionArg::Disabled => Self::Disabled,
        }
    }
}
