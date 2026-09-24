//! Target selector CLI grammar (#24 §17): exactly one selector, no
//! "looks like a UUID/path" heuristic anywhere.

use brainprint_core::{ResourceId, SymbolId, protocol::query::*};
use clap::Args;

use super::vocab::{LanguageArg, SymbolKindArg};

#[derive(Debug, Clone, Default, Args)]
pub struct TargetArgs {
    #[arg(long)]
    pub resource_id: Option<ResourceId>,
    #[arg(long)]
    pub resource_path: Option<String>,
    #[arg(long)]
    pub resource_basename: Option<String>,
    #[arg(long)]
    pub resource_prefix: Option<String>,
    #[arg(long)]
    pub symbol_id: Option<SymbolId>,
    #[arg(long)]
    pub qualified: Option<String>,
    #[arg(long)]
    pub symbol_name: Option<String>,
    #[arg(long)]
    pub partial_symbol: Option<String>,
    /// The complete `ProjectionTargetWire` as JSON; mutually exclusive
    /// with every selector above (#24 §17). The only way to reach a rare
    /// `GraphEndpoint` form (External/Domain/Logical).
    #[arg(long)]
    pub target_json: Option<String>,

    /// Valid only alongside a Symbol selector (`--qualified`,
    /// `--symbol-name`, `--partial-symbol`, `--symbol-id`).
    #[arg(long)]
    pub in_resource: Option<ResourceId>,
    #[arg(long, value_enum)]
    pub symbol_kind: Option<SymbolKindArg>,
    #[arg(long, value_enum)]
    pub language: Option<LanguageArg>,
}

#[derive(Debug)]
pub enum TargetArgsError {
    NoSelector,
    MultipleSelectors,
    /// `--in-resource`/`--symbol-kind`/`--language` without a Symbol
    /// selector.
    SymbolFilterWithoutSymbolSelector,
    InvalidTargetJson(serde_json::Error),
}

impl TargetArgs {
    pub fn into_wire(self) -> Result<ProjectionTargetWire, TargetArgsError> {
        if let Some(json) = self.target_json {
            let has_other = self.resource_id.is_some()
                || self.resource_path.is_some()
                || self.resource_basename.is_some()
                || self.resource_prefix.is_some()
                || self.symbol_id.is_some()
                || self.qualified.is_some()
                || self.symbol_name.is_some()
                || self.partial_symbol.is_some();
            if has_other {
                return Err(TargetArgsError::MultipleSelectors);
            }
            return serde_json::from_str(&json).map_err(TargetArgsError::InvalidTargetJson);
        }

        let symbol_name = [
            self.symbol_id.map(SymbolNameWire::Id),
            self.qualified.map(SymbolNameWire::QualifiedName),
            self.symbol_name.map(SymbolNameWire::Name),
            self.partial_symbol.map(SymbolNameWire::PartialName),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

        let resource_selector = [
            self.resource_id.map(ResourceTargetWire::Id),
            self.resource_path.map(ResourceTargetWire::Path),
            self.resource_basename.map(ResourceTargetWire::Basename),
            self.resource_prefix.map(ResourceTargetWire::PathPrefix),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

        let selector_count = symbol_name.len() + resource_selector.len();
        if selector_count == 0 {
            return Err(TargetArgsError::NoSelector);
        }
        if selector_count > 1 {
            return Err(TargetArgsError::MultipleSelectors);
        }

        if let Some(name) = symbol_name.into_iter().next() {
            return Ok(ProjectionTargetWire::Symbol(SymbolTargetWire {
                name,
                resource: self.in_resource,
                kind: self.symbol_kind.map(Into::into),
                language: self.language.map(Into::into),
            }));
        }

        if self.in_resource.is_some() || self.symbol_kind.is_some() || self.language.is_some() {
            return Err(TargetArgsError::SymbolFilterWithoutSymbolSelector);
        }

        Ok(ProjectionTargetWire::Resource(
            resource_selector
                .into_iter()
                .next()
                .expect("selector_count == 1"),
        ))
    }
}
