//! The rust-analyzer semantic backend.
//!
//! ```text
//! *.rs            (the canonical Resource, always)
//!         ↓ I2/I3 structural tier
//!         ↓ rust-analyzer, at an executable someone named
//!         ↓ adapter
//! SemanticEvidence  (#19 task 1)
//!         ↓ SemanticIndex candidate  (#19 task 3)
//!         ↓ merge                    (#19 task 4)
//! the one canonical Brainprint graph
//! ```
//!
//! The public LSP boundary, and nothing below it: no `ra_ap_*` crate is
//! linked, no salsa identity, crate id, virtual file or syntax pointer
//! is read, and none of them becomes canonical identity.
//!
//! ## Trust: loading a Cargo project reads what the project wrote
//!
//! The same decision C# made, in Rust's words. `build.rs` and
//! procedural macros are code the Workspace chose, and running them to
//! answer a question about source is not a thing a Workspace gets by
//! default. So [`ProjectExecutionTrust`] is untrusted until something
//! says otherwise, and even a *trusted* Workspace is analysed with
//! build scripts, procedural macros and flycheck off — see
//! [`protocol::safe_configuration`]. That was verified by consequence:
//! the fixture's marker `build.rs` did not run and `target/` was never
//! created.
//!
//! One switch that looks like a fourth control is deliberately not set,
//! and the reason is in [`protocol::safe_configuration`].
//!
//! ## What Rust does not have
//!
//! There is no class inheritance, so there is no `EXTENDS` here and no
//! `OVERRIDES`. A trait implementation is `IMPLEMENTS`, which is what
//! the language calls it. A default trait method that an impl replaces
//! is still an implementation of that member, not an override of it,
//! and saying otherwise would import a concept from another language to
//! fill a relation kind that happened to be free.

pub mod adapter;
pub mod host;
pub mod launcher;
pub mod lifecycle;
pub mod protocol;

use std::{collections::BTreeSet, fmt, fs, path::Path};

use brainprint_core::ResourceId;
use rusqlite::Connection;

pub use adapter::{
    AdapterError, LeaseQueries, MappingFailure, Normalizer, ResourceEvidence, ResourceRequest,
    RustQueries, crate_identity, external_entity,
};
pub use host::{RustHost, RustSettings};
pub use launcher::{InstallError, RustInstall, RustLauncher};
pub use lifecycle::{
    BackendReadiness, ChangeClass, ChangeKind, ChangePlan, EnvironmentAssurance,
    EnvironmentIdentity, LifecycleError, OwnerOutcome, ResourceChange, RustProjectConfig,
    SemanticAvailability,
};
pub use protocol::{
    COMPATIBILITY_CLASS, ProjectExecutionTrust, ProtocolCompatibility, TESTED_SERVER_VERSION,
};

use crate::{
    generation::{self, GenerationError},
    merge::{self, MergeError, MergeOutcome, MergeRequest},
    resolution::Support,
    resource::Resource,
    semantic::{
        AnalysisContext, CapabilityReport, SemanticCapability, SemanticEvidence, SemanticOutcome,
        ToolchainIdentity,
    },
    semantic_index::{
        ConfigBasis, CurrentInputs, SemanticBasis, SemanticIndex, SemanticIndexError,
        SemanticOwner, SemanticPublication,
    },
    symbol::{self, AnalysisProfile},
};

// ---------------------------------------------------------------------
// Capability report
// ---------------------------------------------------------------------

/// How well a trait implementation is reported.
///
/// SUPPORTED, and the central Rust capability. The type-level edge
/// comes from the base-list site the structural tier already anchors;
/// the member-level edge comes from the compiler, asked once per trait
/// member. The fixture makes a name-based derivation fail loudly:
/// `Worker` implements both `Runner` and `Reporter`, both declare
/// `run`, and the two implementing declarations share the qualified
/// name `Worker::run`.
pub const IMPLEMENTS: Support = Support::Supported;

/// How well class inheritance is reported.
///
/// UNSUPPORTED, because Rust has none. A supertrait
/// (`trait Detailed: Runner`) is a *requirement* on implementors, not a
/// base class, and `EXTENDS` would state something the language does
/// not have. Leaving it unsupported costs a relation nobody can
/// truthfully draw; overloading it would cost the meaning of every
/// `EXTENDS` edge in the graph.
pub const INHERITANCE: Support = Support::Unsupported;

/// How well method overriding is reported.
///
/// UNSUPPORTED, for the same reason. An impl method that replaces a
/// default trait method implements that member; it does not override
/// it, and there is no dispatch hierarchy for it to override through.
/// `IMPLEMENTS` already says the true thing.
pub const OVERRIDES: Support = Support::Unsupported;

/// How well overload resolution is reported.
///
/// UNSUPPORTED, because Rust has no user-defined function overloading.
/// Method *resolution* does choose among inherent and trait candidates,
/// and that is not overloading: it is a single applicable item found by
/// name and receiver type, which [`SemanticCapability::SymbolDefinition`]
/// already covers. Claiming overload support because the answer is
/// sometimes hard to compute would be claiming a language feature.
pub const OVERLOAD_RESOLUTION: Support = Support::Unsupported;

/// How well a `use` binds.
///
/// PARTIAL. A single-item `use`, an alias, and a named re-export all
/// resolve exactly. What does not is a *glob* — `use crate::prelude::*`
/// names no item, and enumerating what it brings in would mean
/// reimplementing name resolution, which is the backend's job and not
/// this tier's. A compound `use crate::a::{B, C}` is anchored by the
/// structural tier as one occurrence over the whole specifier, so it
/// binds to at most one of its items.
pub const IMPORT_BINDING: Support = Support::Partial;

/// How well a type reference resolves.
///
/// PARTIAL, and the limitation is the structural tier's. Base lists,
/// field types, parameter types and return types are anchored and
/// resolve. A generic *argument* is not anchored, and neither is a
/// bound written in a `where` clause, so there is nothing for this tier
/// to be asked about and those stay gaps rather than being parsed out
/// of signature text.
pub const TYPE_RESOLUTION: Support = Support::Partial;

/// What the Rust backend actually answers, for a given trust mode.
///
/// Declared from measured behaviour over the acceptance fixture, never
/// from the server's provider list: it advertises twenty-two providers
/// and this tier wires up three.
#[must_use]
pub fn capability_report(
    context: &AnalysisContext,
    trust: ProjectExecutionTrust,
) -> CapabilityReport {
    let mut report = CapabilityReport::new(context);

    // ---- Resource / structure: the structural tier's, either way ----
    report
        .declare(SemanticCapability::ResourceDiscovery, Support::Supported)
        .declare(SemanticCapability::SyntaxStructure, Support::Supported)
        .declare(SemanticCapability::SymbolSpan, Support::Supported)
        .declare(SemanticCapability::ContainingScope, Support::Supported)
        // `use` and `pub use` are recorded as written; what they bind
        // to is a separate capability.
        .declare(SemanticCapability::ImportDeclaration, Support::Supported)
        .declare(SemanticCapability::ExportDeclaration, Support::Supported)
        // Rust source is not generated from anything and embeds no
        // other language. Macro expansion is the nearest thing, and it
        // is refused rather than mapped: see
        // [`MappingFailure`](adapter::MappingFailure).
        .declare(
            SemanticCapability::EmbeddedRegionMapping,
            Support::Unsupported,
        )
        .declare(
            SemanticCapability::OriginalSourceMapping,
            Support::Unsupported,
        );

    if !trust.may_load_projects() {
        // Measured: with no Cargo project loaded there is no crate
        // graph, and a crate graph is what every binding needs. What
        // survives is the structural tier, which needs no compiler.
        for capability in [
            SemanticCapability::SymbolDefinition,
            SemanticCapability::ImportBinding,
            SemanticCapability::AliasResolution,
            SemanticCapability::ReexportResolution,
            SemanticCapability::References,
            SemanticCapability::CallsIntraFile,
            SemanticCapability::CallsCrossFile,
            SemanticCapability::TypeResolution,
            SemanticCapability::Inheritance,
            SemanticCapability::Implements,
            SemanticCapability::Overrides,
            SemanticCapability::OverloadResolution,
            SemanticCapability::ImplementationTarget,
            SemanticCapability::ExternalSymbolResolution,
            SemanticCapability::StaticDispatchTarget,
        ] {
            report.declare(capability, Support::Unsupported);
        }
        return report;
    }

    report
        .declare(SemanticCapability::SymbolDefinition, Support::Supported)
        .declare(SemanticCapability::ImportBinding, IMPORT_BINDING)
        // `use … as Alias` reaches the declaration the alias stands for.
        .declare(SemanticCapability::AliasResolution, Support::Supported)
        // `pub use crate::model::Model` — a consumer reaching the
        // re-exported name lands on the real declaration.
        .declare(SemanticCapability::ReexportResolution, Support::Supported)
        .declare(SemanticCapability::References, Support::Supported)
        .declare(SemanticCapability::CallsIntraFile, Support::Supported)
        .declare(SemanticCapability::CallsCrossFile, Support::Supported)
        .declare(SemanticCapability::TypeResolution, TYPE_RESOLUTION)
        // Rust has neither of these; see the constants.
        .declare(SemanticCapability::Inheritance, INHERITANCE)
        .declare(SemanticCapability::Overrides, OVERRIDES)
        .declare(SemanticCapability::OverloadResolution, OVERLOAD_RESOLUTION)
        .declare(SemanticCapability::Implements, IMPLEMENTS)
        .declare(SemanticCapability::ImplementationTarget, Support::Supported)
        // A dependency or a std item becomes a crate identity, and its
        // source is never indexed.
        .declare(
            SemanticCapability::ExternalSymbolResolution,
            Support::Supported,
        )
        // Measured: a concrete call resolves into an `impl`, a `dyn`
        // call into the `trait`. So a statically bound call is
        // identifiable, which is what this capability claims.
        .declare(SemanticCapability::StaticDispatchTarget, Support::Supported);
    report
}

// ---------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------

/// The toolchain a Rust semantic result depends on.
#[must_use]
pub fn toolchain_identity(
    install: &RustInstall,
    environment: &EnvironmentIdentity,
) -> ToolchainIdentity {
    ToolchainIdentity {
        backend_version: install.server_version.clone(),
        backend_compatibility_class: COMPATIBILITY_CLASS.to_owned(),
        environment_fingerprint: environment.fingerprint.clone(),
    }
}

// ---------------------------------------------------------------------
// Refresh
// ---------------------------------------------------------------------

/// Why a refresh produced nothing.
#[derive(Debug)]
pub enum RustSemanticError {
    UnknownResource(ResourceId),
    UnreadableSource { path_rel: String, detail: String },
    Backend(AdapterError),
    Index(SemanticIndexError),
    Merge(MergeError),
    Sqlite(rusqlite::Error),
}

impl fmt::Display for RustSemanticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownResource(resource) => {
                write!(formatter, "{resource} is not an active Resource")
            }
            Self::UnreadableSource { path_rel, detail } => {
                write!(formatter, "could not read {path_rel}: {detail}")
            }
            Self::Backend(error) => write!(formatter, "rust semantic backend: {error}"),
            Self::Index(error) => write!(formatter, "semantic publication: {error}"),
            Self::Merge(error) => write!(formatter, "semantic merge: {error}"),
            Self::Sqlite(error) => write!(formatter, "index: {error}"),
        }
    }
}

impl std::error::Error for RustSemanticError {}

impl From<AdapterError> for RustSemanticError {
    fn from(error: AdapterError) -> Self {
        Self::Backend(error)
    }
}
impl From<SemanticIndexError> for RustSemanticError {
    fn from(error: SemanticIndexError) -> Self {
        Self::Index(error)
    }
}
impl From<MergeError> for RustSemanticError {
    fn from(error: MergeError) -> Self {
        Self::Merge(error)
    }
}
impl From<rusqlite::Error> for RustSemanticError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
impl From<GenerationError> for RustSemanticError {
    fn from(error: GenerationError) -> Self {
        Self::Index(SemanticIndexError::from(error))
    }
}
impl From<crate::symbol::SymbolError> for RustSemanticError {
    fn from(error: crate::symbol::SymbolError) -> Self {
        Self::Index(SemanticIndexError::from(error))
    }
}
impl From<crate::evidence::EvidenceError> for RustSemanticError {
    fn from(error: crate::evidence::EvidenceError) -> Self {
        Self::Sqlite(rusqlite::Error::InvalidParameterName(error.to_string()))
    }
}

/// One Resource's semantic refresh.
pub struct RefreshRequest<'a> {
    pub context: &'a AnalysisContext,
    /// Where the Workspace is now. A locator, never identity.
    pub workspace_root: &'a Path,
    pub owner: ResourceId,
    pub config: &'a ConfigBasis,
    pub capabilities: &'a CapabilityReport,
}

/// What a refresh published and merged.
#[derive(Debug)]
pub struct RefreshOutcome {
    pub publication: SemanticPublication,
    pub merged: MergeOutcome,
    pub deferred: Vec<adapter::DeferredGap>,
    /// Locations refused as generated or virtual, and why.
    pub refused: Vec<(MappingFailure, String)>,
    /// Sites the backend withdrew rather than answered.
    pub withdrawn: Vec<crate::evidence::OccurrenceRef>,
    pub evidence_count: usize,
    /// One line per piece of evidence. Diagnostic only.
    pub report: Vec<String>,
}

/// Resolve one Resource's semantic sites, publish them as a candidate
/// generation, and merge them into the canonical graph.
///
/// The Resource must already be synchronized — see
/// [`lifecycle::synchronize_documents`] for a source change and
/// [`lifecycle::reload_projects`] for a manifest one. Neither is done
/// here, because one batch covers a whole change set.
///
/// # Errors
/// When the backend cannot answer, the publication is refused, or the
/// merge is.
pub fn refresh_resource(
    index: &SemanticIndex,
    queries: &dyn RustQueries,
    request: &RefreshRequest<'_>,
) -> Result<RefreshOutcome, RustSemanticError> {
    let connection = index.connection();
    let context_key = request.context.context_key();

    let owner = adapter::resource_by_id(connection, request.owner)
        .map_err(RustSemanticError::Sqlite)?
        .ok_or(RustSemanticError::UnknownResource(request.owner))?;
    let owner_text =
        fs::read_to_string(request.workspace_root.join(&owner.path_rel)).map_err(|error| {
            RustSemanticError::UnreadableSource {
                path_rel: owner.path_rel.clone(),
                detail: error.to_string(),
            }
        })?;
    let occurrences = symbol::list_occurrences_for_resource(connection, owner.id)?;
    let mut gaps = crate::evidence::list_unresolved_for_resource(connection, owner.id)?;
    for held in adapter::displaced_gaps(connection, &context_key, owner.id)? {
        if !gaps.iter().any(|open| open.occurrence == held.occurrence) {
            gaps.push(held);
        }
    }
    let sites = adapter::collect_sites(connection, &owner, &owner_text, &gaps, &context_key)?;

    let profile = AnalysisProfile::semantic(request.context, request.capabilities);
    let analysis_profile_id = symbol::ensure_profile(connection, &profile)?;

    // Which type each `impl Trait for Type` implements. Read from the
    // parse tree with I3's own derivation, because the trait reference
    // sits outside every declaration and the enclosing-symbol rule
    // would make the *file* the implementor.
    let own_symbols = symbol::list_for_resource(connection, owner.id)?;
    let implementors = crate::parser::dialect_for_path(&owner.path_rel)
        .and_then(|dialect| {
            crate::parser::ParserRegistry::new().parse(
                dialect,
                owner_text.as_bytes(),
                crate::parser::SourceBasis::of(&owner),
            )
        })
        .map(|tree| crate::types::rust_implementors(&tree, owner_text.as_bytes(), &own_symbols))
        .unwrap_or_default();

    let mut normalizer = adapter::Normalizer::new(connection, request.workspace_root);
    let resource_request = adapter::ResourceRequest {
        owner: &owner,
        owner_text: &owner_text,
        implementors: &implementors,
        sites: &sites,
        occurrences: &occurrences,
        generation_id: 0,
        analysis_profile_id,
        context_key: &context_key,
    };
    let mut produced = adapter::resolve_resource(queries, &mut normalizer, &resource_request)?;

    // Which trait member each implementing declaration satisfies. Asked
    // from the trait's side, because that is the direction the backend
    // answers, and anchored on the implementing declaration, because
    // that is the Resource whose replacement must withdraw it.
    let implemented = produced.implemented.clone();
    let mut members =
        adapter::derive_trait_members(queries, &mut normalizer, &resource_request, &implemented)?;
    produced.evidence.append(&mut members);
    produced.refused = normalizer.refused().to_vec();
    let extra_sources = normalizer.sources_read().clone();

    let mut basis = SemanticBasis::new(request.context, request.config, owner.id)
        .with_source(owner.id, owner.resource_revision.clone());
    for (resource, revision) in extra_sources {
        basis = basis.with_source(resource, revision);
    }
    basis = basis.with_inventory(lifecycle::inventory_fingerprint(connection)?);

    let candidate = index.begin_candidate(basis, profile.clone(), Support::Supported)?;
    let generation_id = candidate.generation_id();
    for item in &mut produced.evidence {
        item.basis.generation_id = generation_id;
    }

    let current = CurrentInputs::new(request.context, request.config, request.capabilities)
        .with_inventory(lifecycle::inventory_fingerprint(connection)?);
    let publication = index.publish(candidate, &current)?;

    let status = index.status(&SemanticOwner::new(&context_key, owner.id))?;
    let merged = merge_evidence(
        connection,
        request,
        &owner,
        analysis_profile_id,
        &status,
        &produced.evidence,
    )?;

    let report = produced
        .evidence
        .iter()
        .map(|item| {
            format!(
                "{:?} @{}..{} -> {}",
                item.capability,
                item.occurrence.map_or(0, |site| site.start_byte),
                item.occurrence.map_or(0, |site| site.end_byte),
                match &item.outcome {
                    SemanticOutcome::Resolved { target } =>
                        format!("RESOLVED {:?}", target.entity_kind()),
                    SemanticOutcome::Candidates { targets } =>
                        format!("CANDIDATES {}", targets.len()),
                    SemanticOutcome::Unresolved { reason } => format!("UNRESOLVED {reason:?}"),
                }
            )
        })
        .collect();

    Ok(RefreshOutcome {
        publication,
        merged,
        evidence_count: produced.evidence.len(),
        withdrawn: produced.withdrawn,
        refused: produced.refused,
        deferred: produced.deferred,
        report,
    })
}

/// Apply one contribution inside its own publication transaction.
fn merge_evidence(
    connection: &Connection,
    request: &RefreshRequest<'_>,
    owner: &Resource,
    analysis_profile_id: i64,
    status: &crate::semantic_index::SemanticStatus,
    evidence: &[SemanticEvidence],
) -> Result<MergeOutcome, RustSemanticError> {
    let revision = generation::current_workspace_revision(connection)?
        .ok_or(GenerationError::ClockNotBootstrapped)?;
    let building = generation::begin_generation(connection, &revision)?;
    let transaction = connection.unchecked_transaction()?;
    let (record, grant) = generation::grant_publication(&transaction, building.id)?;

    let outcome = merge::merge(
        &transaction,
        &grant,
        &MergeRequest {
            context: request.context,
            status,
            owner: owner.id,
            analysis_profile_id,
            evidence,
        },
    );
    match outcome {
        Ok(outcome) => {
            generation::finish_publish_stable(&transaction, &record)?;
            transaction.commit()?;
            Ok(outcome)
        }
        Err(error) => {
            drop(transaction);
            generation::abort_generation(connection, building.id, &error.to_string())?;
            Err(RustSemanticError::Merge(error))
        }
    }
}

/// Every capability, for a report that must be complete.
#[must_use]
pub fn declared_capabilities() -> BTreeSet<SemanticCapability> {
    SemanticCapability::ALL.into_iter().collect()
}

#[cfg(test)]
pub(crate) mod tests_support;

#[cfg(test)]
mod tests;
