//! The C# Roslyn semantic backend.
//!
//! ```text
//! *.cs            (the canonical Resource, always)
//!         ↓ I2/I3 structural tier
//!         ↓ Microsoft.CodeAnalysis.LanguageServer  (the official Roslyn LSP)
//!         ↓ adapter
//! SemanticEvidence  (#19 task 1)
//!         ↓ SemanticIndex candidate  (#19 task 3)
//!         ↓ merge                    (#19 task 4)
//! the one canonical Brainprint graph
//! ```
//!
//! ## Candidate A, and what made it survive
//!
//! The spike weighed the official server against a dedicated worker
//! built on the public Roslyn APIs. The official server won on the only
//! axis that mattered -- it answers exact overloads, implementations,
//! overrides, cross-project targets and framework metadata correctly
//! today -- and it is the same component the vendor ships to editors, so
//! it needs no private API, no reflection into compiler internals and no
//! version-chasing of unstable surface area.
//!
//! Two things it does *not* do are handled here rather than pretended
//! away, and both were locked as design decisions before this code
//! existed.
//!
//! ## Partial types: one meaning, several declarations
//!
//! Asked about `Runner`, the server answers with `Runner.Part1.cs` *and*
//! `Runner.Part2.cs`. That is one type, and the previous model had
//! nowhere to put it: a `Symbol` is one span by design, and an
//! Occurrence binds to exactly one relation.
//!
//! The rule that stayed is the binding rule. What was added is the thing
//! bound *to*: a [`LogicalSymbol`](crate::logical_symbol) -- one
//! language-neutral semantic entity owning several source declarations.
//! One occurrence, one relation, one target; the target just happens to
//! be a group. Source Symbols are untouched and remain the exact
//! editable declarations.
//!
//! ## Trust: loading a project runs the project
//!
//! MSBuild evaluation executes project logic -- custom targets, SDK
//! imports, analyzer assemblies. A Workspace is not trusted for that
//! because it is local, because it is a Git repository, because an Agent
//! is editing it, or because it built before. It is trusted when
//! something says so, and nothing here infers it.
//!
//! Untrusted is not "off". It was measured: document-only operation
//! executes no project code and still answers intra-document definitions
//! and document structure -- see
//! [`protocol::MEASURED_PROJECT_LOAD_EFFECTS`]. So an untrusted
//! Workspace gets a smaller, honest capability set, and the difference
//! is in the [`ConfigBasis`] so a publication cannot outlive the trust
//! mode it was made under.

pub mod adapter;
pub mod host;
pub mod launcher;
pub mod lifecycle;
pub mod protocol;

use std::{collections::BTreeSet, fmt, fs, path::Path};

use brainprint_core::{LogicalSymbolId, ResourceId};
use rusqlite::Connection;

pub use adapter::{
    AdapterError, CSharpQueries, LeaseQueries, Normalizer, ResourceEvidence, ResourceRequest,
    assembly_identity, external_entity,
};
pub use host::{CSharpHost, CSharpSettings};
pub use launcher::{CSharpInstall, CSharpLauncher, InstallError};
pub use lifecycle::{
    BackendReadiness, CSharpProjectConfig, ChangeClass, ChangeKind, ChangePlan,
    EnvironmentAssurance, EnvironmentIdentity, LifecycleError, OwnerOutcome, ResourceChange,
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

/// How exactly an overloaded call resolves.
///
/// SUPPORTED, measured. `Parse("1")`, `Parse(1)` and `Parse(new
/// object())` each resolved to their own declaration, and the generic
/// `Convert<T>` to its own -- not to a candidate set and not to the
/// first declaration with the right name. This is the capability C#
/// exists to exercise, and it is the one a name-based tier cannot fake.
pub const OVERLOAD_RESOLUTION: Support = Support::Supported;

/// How well an interface member reaches its implementations.
///
/// SUPPORTED, measured, including the explicit form: `void
/// IRunner.Run()` is found as an implementation of `IRunner.Run` even
/// though it is not callable by that name on the type.
pub const IMPLEMENTATION_TARGET: Support = Support::Supported;

/// How well a `using` directive binds.
///
/// UNSUPPORTED, and not for want of asking. A namespace is not declared
/// in one place -- `namespace Contracts` may be written in twenty files
/// -- and the measured server answers a `using` with nothing at all,
/// which is the correct answer. Rather than bind it to an arbitrary one
/// of those files, the site stays a gap. Type references *into* the
/// namespace resolve normally, which is where the real edges are.
pub const NAMESPACE_IMPORT_BINDING: Support = Support::Unsupported;

/// What the C# backend actually answers, for a given trust mode.
///
/// Declared from measured behaviour over the acceptance fixture, never
/// from the server's provider list: it advertises twenty-one providers
/// and this tier wires up four. Every SUPPORTED entry below is asserted
/// end-to-end in `crates/engine/tests/i4_csharp_acceptance.rs`, through
/// the ordinary query APIs rather than out of the backend.
///
/// Under [`ProjectExecutionTrust::Untrusted`] almost everything
/// collapses to UNSUPPORTED, because it was measured to: with no
/// project loaded the server answers intra-document definitions and
/// document structure and nothing else. That is a narrower report, not
/// a quieter one -- a caller reading it sees exactly what it cannot ask.
///
/// ## Who owns what
///
/// The structural tier (I2/I3) owns discovery, spans, scopes and the
/// declaration of imports; this backend owns every binding. Where a
/// capability is PARTIAL the limitation is one of the two, and it is
/// named at the constant that declares it.
#[must_use]
pub fn capability_report(
    context: &AnalysisContext,
    trust: ProjectExecutionTrust,
) -> CapabilityReport {
    let mut report = CapabilityReport::new(context);

    // ---- Resource / structure: the structural tier's, either way ----
    //
    // None of these needs a compiler, so none of them changes with
    // trust. A C# file is discovered, parsed, and its declarations
    // given spans and scopes with no .NET installed at all.
    report
        .declare(SemanticCapability::ResourceDiscovery, Support::Supported)
        .declare(SemanticCapability::SyntaxStructure, Support::Supported)
        .declare(SemanticCapability::SymbolSpan, Support::Supported)
        .declare(SemanticCapability::ContainingScope, Support::Supported)
        // A `using` directive is recorded as written. What it *binds to*
        // is a separate capability, below.
        .declare(SemanticCapability::ImportDeclaration, Support::Supported)
        // C# has no export syntax; visibility is a modifier on the
        // declaration, which `SymbolSpan` already carries. Claiming this
        // would be a capability with no language behind it.
        .declare(SemanticCapability::ExportDeclaration, Support::Unsupported)
        // C# source is not generated from anything and embeds no other
        // language, so there is no region to map and no original to map
        // back to.
        .declare(
            SemanticCapability::EmbeddedRegionMapping,
            Support::Unsupported,
        )
        .declare(
            SemanticCapability::OriginalSourceMapping,
            Support::Unsupported,
        );

    if !trust.may_load_projects() {
        report
            // Measured: a definition inside the same document resolves
            // with no project loaded, because it needs only the syntax
            // tree the server parses for itself.
            .declare(SemanticCapability::SymbolDefinition, Support::Partial);
        for capability in [
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
        // A namespace has no single declaration; see
        // [`NAMESPACE_IMPORT_BINDING`].
        .declare(SemanticCapability::ImportBinding, NAMESPACE_IMPORT_BINDING)
        .declare(SemanticCapability::AliasResolution, ALIAS_RESOLUTION)
        // C# has no re-export: a type is visible where it is declared,
        // and `using` imports into a file rather than re-publishing.
        .declare(SemanticCapability::ReexportResolution, Support::Unsupported)
        .declare(SemanticCapability::References, Support::Supported)
        .declare(SemanticCapability::CallsIntraFile, Support::Supported)
        .declare(SemanticCapability::CallsCrossFile, Support::Supported)
        .declare(SemanticCapability::TypeResolution, TYPE_RESOLUTION)
        .declare(SemanticCapability::Inheritance, Support::Supported)
        .declare(SemanticCapability::Implements, Support::Supported)
        .declare(SemanticCapability::Overrides, Support::Supported)
        .declare(SemanticCapability::OverloadResolution, OVERLOAD_RESOLUTION)
        .declare(
            SemanticCapability::ImplementationTarget,
            IMPLEMENTATION_TARGET,
        )
        // A framework or package target becomes an assembly identity and
        // its source is never indexed.
        .declare(
            SemanticCapability::ExternalSymbolResolution,
            Support::Supported,
        )
        .declare(SemanticCapability::StaticDispatchTarget, Support::Supported);
    report
}

/// How well a `using Alias = Some.Type;` binds.
///
/// PARTIAL. The aliased type resolves, by chaining two answers the
/// compiler gave -- the use reaches the alias declaration, and the
/// declaration's own right-hand side reaches the type. What is not
/// covered is `using static`, which names a type whose *members* enter
/// scope rather than a single target, and an alias written across
/// several lines, which the positional join between the two answers
/// does not reach. Both stay gaps.
pub const ALIAS_RESOLUTION: Support = Support::Partial;

/// How well a type reference resolves.
///
/// PARTIAL, and the limitation is the structural tier's rather than the
/// backend's. Every type reference I3 anchors an Occurrence for --
/// a base list, a field or property type, a parameter type --
/// resolves. A *return* type and a generic type argument are not
/// anchored: `Box<Model> Wrap(Model model)` records one type site, on
/// the parameter. There is nothing for this tier to be asked about, so
/// those stay unresolved rather than being guessed from the signature
/// text.
pub const TYPE_RESOLUTION: Support = Support::Partial;

// ---------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------

/// The toolchain a C# semantic result depends on.
///
/// The version comes from the install manifest rather than the
/// handshake, because an [`AnalysisContext`] exists before a runtime is
/// selected. The trust mode is inside
/// [`EnvironmentIdentity::fingerprint`], so a trusted and an untrusted
/// analysis of the same Workspace are two different toolchain
/// identities, which is what makes granting trust invalidate rather than
/// silently widen.
#[must_use]
pub fn toolchain_identity(
    install: &CSharpInstall,
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
pub enum CSharpSemanticError {
    UnknownResource(ResourceId),
    UnreadableSource {
        path_rel: String,
        detail: String,
    },
    Backend(AdapterError),
    Index(SemanticIndexError),
    Merge(MergeError),
    Sqlite(rusqlite::Error),
    /// No stable generation exists yet, so a group has nothing to record
    /// itself as created during.
    ClockNotBootstrapped,
}

impl fmt::Display for CSharpSemanticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownResource(resource) => {
                write!(formatter, "{resource} is not an active Resource")
            }
            Self::UnreadableSource { path_rel, detail } => {
                write!(formatter, "could not read {path_rel}: {detail}")
            }
            Self::Backend(error) => write!(formatter, "c# semantic backend: {error}"),
            Self::Index(error) => write!(formatter, "semantic publication: {error}"),
            Self::Merge(error) => write!(formatter, "semantic merge: {error}"),
            Self::Sqlite(error) => write!(formatter, "index: {error}"),
            Self::ClockNotBootstrapped => {
                write!(formatter, "no stable generation to attribute a group to")
            }
        }
    }
}

impl std::error::Error for CSharpSemanticError {}

impl From<AdapterError> for CSharpSemanticError {
    fn from(error: AdapterError) -> Self {
        Self::Backend(error)
    }
}
impl From<SemanticIndexError> for CSharpSemanticError {
    fn from(error: SemanticIndexError) -> Self {
        Self::Index(error)
    }
}
impl From<MergeError> for CSharpSemanticError {
    fn from(error: MergeError) -> Self {
        Self::Merge(error)
    }
}
impl From<rusqlite::Error> for CSharpSemanticError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
impl From<GenerationError> for CSharpSemanticError {
    fn from(error: GenerationError) -> Self {
        Self::Index(SemanticIndexError::from(error))
    }
}
impl From<crate::symbol::SymbolError> for CSharpSemanticError {
    fn from(error: crate::symbol::SymbolError) -> Self {
        Self::Index(SemanticIndexError::from(error))
    }
}
impl From<crate::evidence::EvidenceError> for CSharpSemanticError {
    fn from(error: crate::evidence::EvidenceError) -> Self {
        Self::Sqlite(rusqlite::Error::InvalidParameterName(error.to_string()))
    }
}
fn logical(error: crate::graph::GraphError) -> CSharpSemanticError {
    CSharpSemanticError::Sqlite(rusqlite::Error::InvalidParameterName(error.to_string()))
}

impl From<crate::logical_symbol::LogicalSymbolError> for CSharpSemanticError {
    fn from(error: crate::logical_symbol::LogicalSymbolError) -> Self {
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
    /// Which project owns which Resource, for logical identity.
    pub projects: &'a CSharpProjectConfig,
}

/// What a refresh published and merged.
#[derive(Debug)]
pub struct RefreshOutcome {
    pub publication: SemanticPublication,
    pub merged: MergeOutcome,
    pub deferred: Vec<adapter::DeferredGap>,
    /// Sites the backend withdrew rather than answered. Coverage this
    /// pass did not reach, recorded rather than guessed at.
    pub withdrawn: Vec<crate::evidence::OccurrenceRef>,
    pub evidence_count: usize,
    /// The logical symbols this pass proved.
    pub grouped: BTreeSet<LogicalSymbolId>,
    /// Declarations that look like an override or an implementation and
    /// could not be proven to be one. Reported, never guessed at.
    pub unproven: Vec<crate::semantic_overrides::UnprovenOverride>,
    /// One line per piece of evidence. Diagnostic only.
    pub report: Vec<String>,
}

/// Resolve one Resource's semantic sites, publish them as a candidate
/// generation, and merge them into the canonical graph.
///
/// Evidence is collected before the candidate opens, because the task 3
/// basis has to name every Resource the analysis read and
/// [`SemanticIndex::publish`] revalidates the whole basis inside the
/// publishing transaction.
///
/// The Resource must already be synchronized -- see
/// [`lifecycle::synchronize_documents`] for a content change and
/// [`lifecycle::reload_projects`] for a structural one. Neither is done
/// here, because one batch covers a whole change set.
///
/// # Errors
/// When the backend cannot answer, the publication is refused, or the
/// merge is.
pub fn refresh_resource(
    index: &SemanticIndex,
    queries: &dyn CSharpQueries,
    request: &RefreshRequest<'_>,
) -> Result<RefreshOutcome, CSharpSemanticError> {
    let connection = index.connection();
    let context_key = request.context.context_key();

    let owner = adapter::resource_by_id(connection, request.owner)
        .map_err(CSharpSemanticError::Sqlite)?
        .ok_or(CSharpSemanticError::UnknownResource(request.owner))?;
    let owner_text =
        fs::read_to_string(request.workspace_root.join(&owner.path_rel)).map_err(|error| {
            CSharpSemanticError::UnreadableSource {
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

    // A group records the generation it was first observed in. That is
    // provenance, not the publication -- the candidate generation does
    // not exist yet, and a group outlives any single publication anyway.
    let observed_in = generation::current_stable(connection)?
        .map(|record| record.id)
        .ok_or(CSharpSemanticError::ClockNotBootstrapped)?;

    let mut normalizer = adapter::Normalizer::new(
        connection,
        request.workspace_root,
        &context_key,
        &request.projects.owning_project,
        observed_in,
    );
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
    let mut extra_sources = normalizer.sources_read().clone();

    // `OVERRIDES` and member-level `IMPLEMENTS` are not written at a
    // use site, so no gap carries them. Both are derived from edges
    // someone proved -- inheritance and interface lists -- and anchored
    // on the overriding or implementing declaration's own Occurrence.
    // The `override` keyword and the `IRunner.` qualifier are *claims*:
    // they decide which interface a member may match and whether an
    // unprovable claim is worth reporting, never that an edge exists.
    let own_symbols = symbol::list_for_resource(connection, owner.id)?;
    let evidence_basis = crate::resolution::EvidenceBasis {
        owner_resource: owner.id,
        owner_resource_revision: owner.resource_revision.clone(),
        generation_id: 0,
        analysis_profile_id,
        resolution_context_key: None,
    };
    let declared_overrides = adapter::override_members(&owner_text, &own_symbols, &occurrences);
    let mut explicit = adapter::explicit_interface_members(&owner_text, &own_symbols, &occurrences);
    // A partial type's other halves: an explicit `void IRunner.Run()`
    // written there still takes the name here, so the claim has to be
    // read from every declaration of the type.
    for sibling in adapter::sibling_resources(connection, owner.id, &own_symbols)? {
        let Some(resource) = adapter::resource_by_id(connection, sibling)? else {
            continue;
        };
        let Ok(text) = fs::read_to_string(request.workspace_root.join(&resource.path_rel)) else {
            continue;
        };
        explicit.extend(adapter::explicit_interface_members(
            &text,
            &symbol::list_for_resource(connection, resource.id)?,
            &symbol::list_occurrences_for_resource(connection, resource.id)?,
        ));
        extra_sources.insert(resource.id, resource.resource_revision);
    }
    let mut derived = crate::semantic_overrides::derive(
        connection,
        &owner,
        &occurrences,
        &produced.resolved_bases,
        &context_key,
        &evidence_basis,
        &declared_overrides,
    )
    .map_err(logical)?;
    let mut implemented = crate::semantic_overrides::derive_implements(
        connection,
        &owner,
        &occurrences,
        &produced.resolved_bases,
        &context_key,
        &evidence_basis,
        &explicit,
    )
    .map_err(logical)?;
    // An ancestor's declaration is part of the proof, so the basis has
    // to name it: `Runner.Run OVERRIDES BaseRunner.Run` stops being
    // true when the file declaring `BaseRunner` moves.
    for ancestor in derived
        .ancestor_resources
        .iter()
        .chain(&implemented.ancestor_resources)
    {
        if let Some(resource) = adapter::resource_by_id(connection, *ancestor)? {
            extra_sources.insert(resource.id, resource.resource_revision);
        }
    }
    produced.evidence.append(&mut derived.evidence);
    produced.evidence.append(&mut implemented.evidence);
    let unproven: Vec<crate::semantic_overrides::UnprovenOverride> = derived
        .unproven
        .into_iter()
        .chain(implemented.unproven)
        .collect();

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
        unproven,
        grouped: produced.grouped.keys().copied().collect(),
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
) -> Result<MergeOutcome, CSharpSemanticError> {
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
            Err(CSharpSemanticError::Merge(error))
        }
    }
}

#[cfg(test)]
pub(crate) mod tests_support;

#[cfg(test)]
mod tests;
