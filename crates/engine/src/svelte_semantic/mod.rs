//! The Svelte semantic backend.
//!
//! Svelte is a container/backend boundary, and this is that boundary:
//!
//! ```text
//! Component.svelte  (the canonical Resource, always)
//!         ↓ I2/I3 container structure + embedded script (#19 task 11)
//!         ↓ svelte-language-server, which owns svelte2tsx internally
//!         ↓ answers already in ORIGINAL .svelte coordinates
//!         ↓ adapter
//! SemanticEvidence  (#19 task 1)
//!         ↓ SemanticIndex candidate  (#19 task 3)
//!         ↓ merge                    (#19 task 4)
//! the one canonical Brainprint graph
//! ```
//!
//! ## React is not here, and that is the architecture
//!
//! `.tsx` and `.jsx` have no backend of their own. A React component
//! usage is an ordinary reference in an ordinary TypeScript file, and
//! #19 task 10's backend resolves it -- once I3 records an anchor at the
//! JSX element name, which is the whole of task 11's React work. There
//! is no `SemanticBackendKind::React`, no hook rule, no route rule, and
//! no `COMPONENT_USES` relation: `<UserCard />` is
//! `REFERENCES` the component's declaration, because that is what it is.
//!
//! ## What the measurement settled
//!
//! Three things, each of which could have forced a different design.
//!
//! The official server answers in original `.svelte` positions, so
//! Brainprint consumes no source map and never sees a generated
//! coordinate -- and [`adapter`] refuses one if that ever changes.
//!
//! It runs on its own TypeScript 6, because it cannot run on the
//! TypeScript 7 the TS/JS backend uses; see
//! [`protocol::TESTED_TYPESCRIPT_VERSION`].
//!
//! It executes `svelte.config.js` unless told the project is untrusted,
//! so Brainprint tells it exactly that; see [`protocol::IS_TRUSTED`].

pub mod adapter;
pub mod host;
pub mod launcher;
pub mod lifecycle;
pub mod protocol;

use std::{fmt, fs, path::Path};

use brainprint_core::ResourceId;
use rusqlite::Connection;

pub use adapter::{
    AdapterError, LeaseQueries, MappingFailure, Normalizer, ResourceEvidence, ResourceRequest,
    SvelteQueries,
};
pub use host::{SvelteHost, SvelteSettings};
pub use launcher::{InstallError, SvelteInstall, SvelteLauncher};
pub use lifecycle::{
    BackendReadiness, ChangeKind, ChangePlan, EnvironmentAssurance, EnvironmentIdentity,
    LifecycleError, OwnerOutcome, ResourceChange, SemanticAvailability, SvelteProjectConfig,
};
pub use protocol::{COMPATIBILITY_CLASS, ProtocolCompatibility, TESTED_SERVER_VERSION};

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

/// How well a generated position maps back to original source.
///
/// SUPPORTED, and measured rather than assumed -- but measured at a
/// boundary one step further out than the task expected. Brainprint
/// never *sees* a generated position: the official server maps every
/// answer back into `.svelte` coordinates before it returns, for
/// template identifiers, component tags, script types and cross-file
/// targets alike. So the capability is real and the mechanism is the
/// server's.
///
/// What makes this a claim rather than a hope is
/// [`adapter::MappingFailure`]: a URI naming `svelte2tsx` output, a file
/// that is not an indexed Resource, or a range that does not convert
/// exactly, are each refused and reported. Nothing approximate is ever
/// published, so a future version that started leaking generated
/// positions would produce gaps rather than wrong spans.
pub const ORIGINAL_SOURCE_MAPPING: Support = Support::Supported;

/// How well the container's embedded regions are mapped.
///
/// SUPPORTED for what this tier reads: the `<script>` and
/// `<script module>` blocks, and the template expressions and component
/// tags that use what those scripts bind. Every one of those is
/// extracted at the component's own byte offsets by
/// [`crate::svelte_structure`], which is what the semantic answers then
/// anchor to.
///
/// PARTIAL would be the honest word if `<style>` were in scope; it is
/// not, and it declares no Symbol either, so nothing is lost by not
/// claiming it. What *is* outside this claim is recorded in the
/// container's own [`StructuralState::ContainerOnly`](crate::structural::StructuralState).
pub const EMBEDDED_REGION_MAPPING: Support = Support::Supported;

/// How well a module-level script's symbols are covered.
///
/// PARTIAL, measured. `<script module>` declarations are extracted and
/// published with their own spans -- they are real Symbols at real
/// coordinates -- but the language server did not resolve a reference
/// from the instance script *to* a module-script declaration in the
/// fixture. The two scopes are kept apart rather than flattened by name,
/// which is the requirement; what is missing is the binding between
/// them, and it is a gap rather than an invented edge.
pub const MODULE_SCRIPT_SUPPORT: Support = Support::Partial;

/// What the Svelte backend actually answers, as implemented here.
///
/// Declared from measured behaviour over the acceptance fixture, never
/// from the server's provider list: `svelte-language-server` declares
/// twenty-two providers and this tier wires up one.
#[must_use]
pub fn capability_report(context: &AnalysisContext) -> CapabilityReport {
    let mut report = CapabilityReport::new(context);
    report
        // The two capabilities #19 task 1 defined for exactly this
        // moment, and the reason a container backend exists at all.
        .declare(
            SemanticCapability::EmbeddedRegionMapping,
            EMBEDDED_REGION_MAPPING,
        )
        .declare(
            SemanticCapability::OriginalSourceMapping,
            ORIGINAL_SOURCE_MAPPING,
        )
        // A template identifier reaching its script declaration, and a
        // script name reaching a cross-file declaration.
        .declare(SemanticCapability::SymbolDefinition, Support::Supported)
        // `import Child from './lib/Child.svelte'` and
        // `import type { Model } from './lib/model'`.
        .declare(SemanticCapability::ImportBinding, Support::Supported)
        .declare(SemanticCapability::AliasResolution, Support::Supported)
        // The capability this tier exists for: `{increment}` and
        // `<Child>` in the markup reaching what the script bound.
        .declare(SemanticCapability::References, Support::Supported)
        // A script's own calls resolve. PARTIAL rather than SUPPORTED
        // because a call written in the *template* is a reference to the
        // binding, not a call site I3 records as one.
        .declare(SemanticCapability::CallsIntraFile, Support::Partial)
        .declare(SemanticCapability::CallsCrossFile, Support::Partial)
        // `let model: Model` inside `<script lang="ts">` reaches the
        // interface in the `.ts` beside it.
        .declare(SemanticCapability::TypeResolution, Support::Supported)
        // A dependency import from inside a component becomes an
        // identity, and its source is never indexed.
        .declare(
            SemanticCapability::ExternalSymbolResolution,
            Support::Supported,
        )
        // Svelte components do not inherit, implement or override.
        // Claiming any of these would be a capability with no language
        // behind it.
        .declare(SemanticCapability::Inheritance, Support::Unsupported)
        .declare(SemanticCapability::Implements, Support::Unsupported)
        .declare(SemanticCapability::Overrides, Support::Unsupported)
        .declare(SemanticCapability::OverloadResolution, Support::Unsupported)
        .declare(
            SemanticCapability::ImplementationTarget,
            Support::Unsupported,
        )
        // A template writes a binding, not a virtual call.
        .declare(SemanticCapability::StaticDispatchTarget, Support::Partial);
    report
}

// ---------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------

/// The toolchain a Svelte semantic result depends on.
///
/// The backend version comes from the install manifest, not from the
/// handshake, because this server reports no `serverInfo` at all. That
/// is not a workaround: an [`AnalysisContext`] has to exist before a
/// runtime is selected, so every field here must be knowable before
/// anything starts.
#[must_use]
pub fn toolchain_identity(
    install: &SvelteInstall,
    environment: &lifecycle::EnvironmentIdentity,
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
pub enum SvelteSemanticError {
    UnknownResource(ResourceId),
    UnreadableSource { path_rel: String, detail: String },
    Backend(AdapterError),
    Index(SemanticIndexError),
    Merge(MergeError),
    Sqlite(rusqlite::Error),
}

impl fmt::Display for SvelteSemanticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownResource(resource) => {
                write!(formatter, "{resource} is not an active Resource")
            }
            Self::UnreadableSource { path_rel, detail } => {
                write!(formatter, "could not read {path_rel}: {detail}")
            }
            Self::Backend(error) => write!(formatter, "svelte semantic backend: {error}"),
            Self::Index(error) => write!(formatter, "semantic publication: {error}"),
            Self::Merge(error) => write!(formatter, "semantic merge: {error}"),
            Self::Sqlite(error) => write!(formatter, "index: {error}"),
        }
    }
}

impl std::error::Error for SvelteSemanticError {}

impl From<AdapterError> for SvelteSemanticError {
    fn from(error: AdapterError) -> Self {
        Self::Backend(error)
    }
}
impl From<SemanticIndexError> for SvelteSemanticError {
    fn from(error: SemanticIndexError) -> Self {
        Self::Index(error)
    }
}
impl From<MergeError> for SvelteSemanticError {
    fn from(error: MergeError) -> Self {
        Self::Merge(error)
    }
}
impl From<rusqlite::Error> for SvelteSemanticError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
impl From<GenerationError> for SvelteSemanticError {
    fn from(error: GenerationError) -> Self {
        Self::Index(SemanticIndexError::from(error))
    }
}
impl From<crate::symbol::SymbolError> for SvelteSemanticError {
    fn from(error: crate::symbol::SymbolError) -> Self {
        Self::Index(SemanticIndexError::from(error))
    }
}
impl From<crate::evidence::EvidenceError> for SvelteSemanticError {
    fn from(error: crate::evidence::EvidenceError) -> Self {
        Self::Sqlite(rusqlite::Error::InvalidParameterName(error.to_string()))
    }
}

/// One component's semantic refresh.
pub struct RefreshRequest<'a> {
    pub context: &'a AnalysisContext,
    /// Where the Workspace is now. A locator, never identity.
    pub workspace_root: &'a Path,
    /// Always the original `.svelte` Resource. Nothing generated is ever
    /// an owner.
    pub owner: ResourceId,
    pub config: &'a ConfigBasis,
    pub capabilities: &'a CapabilityReport,
}

/// What a refresh published and merged.
#[derive(Debug)]
pub struct RefreshOutcome {
    pub publication: SemanticPublication,
    pub merged: MergeOutcome,
    /// Locations this pass refused to publish, and why. Incomplete
    /// coverage, never a fabricated span.
    pub mapping_failures: Vec<(MappingFailure, String)>,
    pub deferred: Vec<adapter::DeferredGap>,
    pub evidence_count: usize,
    /// One line per piece of evidence. Diagnostic only.
    pub report: Vec<String>,
}

/// Resolve one component's semantic sites, publish them as a candidate
/// generation, and merge them into the canonical graph.
///
/// The ordering is the same one every backend in #19 uses and for the
/// same reason: evidence is collected before the candidate opens,
/// because the task 3 basis has to name every Resource the analysis
/// read, and [`SemanticIndex::publish`] revalidates the whole basis
/// inside the publishing transaction.
///
/// The component must already be synchronized -- see
/// [`lifecycle::synchronize`]. It is not done here, because one batch
/// covers a whole change set and doing it per owner would send one
/// notification per owner.
///
/// # Errors
/// When the backend cannot answer, the publication is refused, or the
/// merge is.
pub fn refresh_resource(
    index: &SemanticIndex,
    queries: &dyn SvelteQueries,
    request: &RefreshRequest<'_>,
) -> Result<RefreshOutcome, SvelteSemanticError> {
    let connection = index.connection();
    let context_key = request.context.context_key();

    let owner = adapter::resource_by_id(connection, request.owner)
        .map_err(SvelteSemanticError::Sqlite)?
        .ok_or(SvelteSemanticError::UnknownResource(request.owner))?;
    let owner_text =
        fs::read_to_string(request.workspace_root.join(&owner.path_rel)).map_err(|error| {
            SvelteSemanticError::UnreadableSource {
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

    let mut normalizer = adapter::Normalizer::new(connection, request.workspace_root);
    let mut produced = adapter::resolve_resource(
        queries,
        &mut normalizer,
        &adapter::ResourceRequest {
            owner: &owner,
            owner_text: &owner_text,
            sites: &sites,
            occurrences: &occurrences,
            generation_id: 0,
            analysis_profile_id,
            context_key: &context_key,
        },
    )?;
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
        mapping_failures: produced.mapping_failures,
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
) -> Result<MergeOutcome, SvelteSemanticError> {
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
            Err(SvelteSemanticError::Merge(error))
        }
    }
}

#[cfg(test)]
pub(crate) mod tests_support;

#[cfg(test)]
mod tests;
