//! The Python semantic backend.
//!
//! Productionizes the transport #19 task 5 settled by measurement: one
//! pinned `pyright-typeserver` child process, one stdio JSON-RPC
//! connection, `HostConcurrency::Serial`, and filesystem truth with no
//! editor overlay. The same process answers the `typeServer/*` snapshot,
//! import and type requests *and* the LSP definition, declaration,
//! reference and call-hierarchy requests, so there is no second backend
//! and no second analysis of the same project.
//!
//! The layering:
//!
//! ```text
//! SemanticRuntimeSupervisor            (#19 task 2, backend-neutral)
//!         ↓ SemanticBackendLauncher
//! PythonLauncher → PyrightHost         (launcher, host, jsonrpc)
//!         ↓ PythonRequest / PythonResponse
//! pyright-typeserver                   (TSP + LSP, one connection)
//!         ↓ adapter
//! SemanticEvidence                     (#19 task 1)
//!         ↓ SemanticIndex candidate    (#19 task 3)
//!         ↓ merge                      (#19 task 4)
//! the one canonical Brainprint graph
//! ```
//!
//! Nothing below the adapter line knows about Pyright, and nothing
//! above it sees a URI, a UTF-16 offset, a snapshot number or a
//! JSON-RPC id. There is deliberately no public `python_definition()`
//! or `tsp_type()` query surface: after the merge, the existing
//! `callers`, `references`, impact and prepared-inspection APIs are
//! what answer, enriched rather than duplicated.
//!
//! ## What this task does not do
//!
//! `IMPLEMENTS`, `OVERRIDES` and inheritance enrichment stay
//! [`Support::Unsupported`]. Task 5 measured both
//! `textDocument/implementation` and `textDocument/prepareTypeHierarchy`
//! answering `MethodNotFound` on this artifact, so those relations have
//! to be *derived* -- which is #19 task 7, with its own evidence.
//! Declaring them here would be a capability claim nothing backs.

pub mod adapter;
pub mod coordinates;
pub mod host;
pub mod jsonrpc;
pub mod launcher;
pub mod lifecycle;
pub mod overrides;
pub mod protocol;

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs,
    path::Path,
};

use brainprint_core::{ResourceId, SymbolId};
use rusqlite::{Connection, params};

pub use adapter::{
    Batch, BatchError, BatchPolicy, LeaseQueries, Normalizer, PythonQueries, ResourceEvidence,
    ResourceRequest, resolve_resource, run_batch,
};
pub use host::{PyrightHost, PythonSettings};
pub use launcher::{InstallError, PyrightInstall, PythonLauncher, Readiness};
pub use lifecycle::{
    BackendReadiness, ChangeKind, ChangePlan, ConfigSource, EnvironmentAssurance,
    EnvironmentIdentity, LifecycleError, OwnerOutcome, PythonProjectConfig, ResourceChange,
    SemanticAvailability,
};
pub use overrides::{Derivation, UnprovenOverride, UnprovenReason};
pub use protocol::{COMPATIBILITY_CLASS, ProtocolCompatibility, TESTED_PROTOCOL_VERSION};

use crate::{
    db,
    generation::{self, GenerationError},
    merge::{self, MergeError, MergeOutcome, MergeRequest},
    resolution::{EvidenceBasis, Support},
    resource::{Resource, ResourceLanguage},
    semantic::{
        AnalysisContext, CapabilityReport, SemanticCapability, SemanticEvidence, ToolchainIdentity,
    },
    semantic_index::{
        ConfigBasis, CurrentInputs, SemanticBasis, SemanticIndex, SemanticIndexError,
        SemanticOwner, SemanticPublication,
    },
    symbol::{self, AnalysisProfile},
};

/// How well `typeServer/getExpectedType` is covered.
///
/// Task 5 measured it answering for an argument or a return position
/// and `null` everywhere else. That is a real partial capability, and
/// rounding it up to SUPPORTED would make an absent expected type read
/// as "there is none".
pub const EXPECTED_TYPE_SUPPORT: Support = Support::Partial;

/// How well a Python base list is covered.
///
/// Every base shape the acceptance fixture writes resolves: plain,
/// qualified (`class Qualified(base.Base)`), cross-file, nested,
/// multiple, and a dependency base as an external identity (#19 task 9
/// measured all six). PARTIAL is about what the fixture does *not*
/// write and this tier does not model: a base that is not a name or an
/// attribute expression -- a subscripted generic, a call, a variable --
/// has no declaration site to anchor an edge to, and task 4 refuses to
/// invent one. So the answer there is an honest gap, not an edge.
pub const INHERITANCE_SUPPORT: Support = Support::Partial;

/// How well `OVERRIDES` is covered. See [`overrides`].
pub const OVERRIDES_SUPPORT: Support = Support::Partial;

/// How well a call site's bound target is covered.
///
/// A direct call binds to a declaration and is recorded `STATIC`. A
/// call through a typed receiver binds to the *declaration the type
/// names*, which is not necessarily the code that runs, so task 7
/// records it `UNKNOWN` rather than claiming a static target. Both are
/// exact; only one of them is the capability, hence PARTIAL. Rounding
/// it up would turn "here is the declaration" into "here is what
/// executes", which Python does not support anyone saying.
pub const STATIC_DISPATCH_SUPPORT: Support = Support::Partial;

// ---------------------------------------------------------------------
// Capability report
// ---------------------------------------------------------------------

/// What the Python backend actually answers, as implemented here.
///
/// Declared from this task's behaviour, not from the backend's
/// theoretical surface: Pyright can do more than task 6 wires up, and
/// claiming the difference would be a coverage claim with no code
/// behind it.
#[must_use]
pub fn capability_report(context: &AnalysisContext) -> CapabilityReport {
    let mut report = CapabilityReport::new(context);
    report
        .declare(SemanticCapability::ImportBinding, Support::Supported)
        // `from .base import Base as Exported`, and the module that
        // then imports `Exported`, both reach `Base` (#19 task 9).
        // Declared from that measurement, not from Pyright's brochure.
        .declare(SemanticCapability::AliasResolution, Support::Supported)
        .declare(SemanticCapability::ReexportResolution, Support::Supported)
        .declare(
            SemanticCapability::ExternalSymbolResolution,
            Support::Supported,
        )
        .declare(SemanticCapability::SymbolDefinition, Support::Supported)
        .declare(SemanticCapability::References, Support::Supported)
        .declare(SemanticCapability::CallsIntraFile, Support::Supported)
        .declare(SemanticCapability::CallsCrossFile, Support::Supported)
        // See [`STATIC_DISPATCH_SUPPORT`]: a target is always exact, a
        // *static* target is only claimed where it is one.
        .declare(
            SemanticCapability::StaticDispatchTarget,
            STATIC_DISPATCH_SUPPORT,
        )
        // Declared and computed types resolve; the expected type does
        // not always, so the capability as a whole is partial.
        .declare(SemanticCapability::TypeResolution, EXPECTED_TYPE_SUPPORT)
        // A base list resolves wherever I3 anchored one. A base written
        // as an attribute expression (`class X(pkg.Base)`) gets no
        // Occurrence from I3 at all, so there is nothing to anchor and
        // that shape stays unresolved -- hence PARTIAL, not SUPPORTED.
        .declare(SemanticCapability::Inheritance, INHERITANCE_SUPPORT)
        // Derived from proven inheritance (#19 task 7). PARTIAL because
        // Python writes `@classmethod`, `@staticmethod` and `@property`
        // as decorators and I3 records all three as METHOD, and because
        // two same-depth ancestors declaring one name is refused rather
        // than resolved by base-list order the graph does not store.
        .declare(SemanticCapability::Overrides, OVERRIDES_SUPPORT)
        // Python has no implements clause, and inferring conformance
        // from matching members is duck typing, not evidence. Pyright's
        // public surface offers no explicit Protocol-conformance fact.
        .declare(SemanticCapability::Implements, Support::Unsupported)
        .declare(
            SemanticCapability::ImplementationTarget,
            Support::Unsupported,
        );
    report
}

// ---------------------------------------------------------------------
// Identity and basis inputs
// ---------------------------------------------------------------------

/// The toolchain a Python semantic result depends on.
///
/// The Node runtime and the install directory are deliberately absent
/// from the environment half: where the type server lives does not
/// change what Python code means, and a machine path in identity would
/// make the same project a different context on every machine. What
/// *is* in it is the dependency-resolution environment
/// ([`lifecycle::environment_identity`]), because the same `.venv` path
/// resolves imports differently after an install.
///
/// Every field is known before anything is started, which it has to be:
/// the [`AnalysisContext`] built from it is what *selects* the runtime,
/// so it cannot depend on a running one. The negotiated protocol
/// version is therefore not an input but a gate --
/// [`PythonLauncher`] refuses to start against a version outside
/// [`COMPATIBILITY_CLASS`], so a publication under this identity can
/// only ever have come from a protocol the adapter has read.
#[must_use]
pub fn toolchain_identity(
    install: &PyrightInstall,
    environment: &lifecycle::EnvironmentIdentity,
) -> ToolchainIdentity {
    ToolchainIdentity {
        backend_version: install.package_version.clone(),
        backend_compatibility_class: COMPATIBILITY_CLASS.to_owned(),
        environment_fingerprint: environment.fingerprint.clone(),
    }
}

/// The configuration a Python semantic result depends on.
///
/// The config file enters by its already-computed content identity, so
/// nothing re-reads or stores source. Task 8 owns discovering which
/// file that is; this only says what happens once it is known.
#[must_use]
pub fn config_basis(settings: &PythonSettings, config_file: Option<&Resource>) -> ConfigBasis {
    let mut basis = ConfigBasis::new().with(
        "pyright_config",
        config_file.map_or_else(String::new, |resource| {
            resource
                .content_hash
                .clone()
                .unwrap_or_else(|| resource.fingerprint.clone())
        }),
    );
    for (name, value) in settings.basis_inputs() {
        if name != "python_path" && name != "venv_path" {
            basis = basis.with(name, value);
        }
    }
    basis
}

/// A deterministic identity for the Python module set a resolution
/// happens against.
///
/// Task 5 found the Pyright snapshot to be program-wide: any file in
/// the project invalidates it, so a result does not depend only on the
/// Resources it named. That is exactly what task 3's
/// `inventory_fingerprint` is for, and why Python must populate it.
///
/// Module-resolution context only -- the sorted path keys of the active
/// Python Resources. No source body is read, so adding a file moves
/// this and editing one does not.
pub fn inventory_fingerprint(
    connection: &Connection,
    language: ResourceLanguage,
) -> Result<String, rusqlite::Error> {
    let mut statement = connection.prepare(
        "SELECT path_key FROM resource \
         WHERE state = 'ACTIVE' AND language = ?1 ORDER BY path_key",
    )?;
    let keys: Vec<String> = statement
        .query_map(params![language.to_string()], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let fields: Vec<(&str, &str)> = keys.iter().map(|key| ("module", key.as_str())).collect();
    Ok(db::fingerprint("python-semantic-inventory-1", &fields))
}

// ---------------------------------------------------------------------
// Refresh
// ---------------------------------------------------------------------

/// Why a refresh produced nothing.
#[derive(Debug)]
pub enum PythonSemanticError {
    /// The Resource is not in the current Workspace inventory.
    UnknownResource(ResourceId),
    /// Its bytes could not be read, so no span could be converted.
    UnreadableSource {
        path_rel: String,
        detail: String,
    },
    Backend(BatchError),
    Index(SemanticIndexError),
    Merge(MergeError),
    Sqlite(rusqlite::Error),
}

impl fmt::Display for PythonSemanticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownResource(resource) => {
                write!(formatter, "{resource} is not an active Resource")
            }
            Self::UnreadableSource { path_rel, detail } => {
                write!(formatter, "could not read {path_rel}: {detail}")
            }
            Self::Backend(error) => write!(formatter, "python semantic backend: {error}"),
            Self::Index(error) => write!(formatter, "semantic publication: {error}"),
            Self::Merge(error) => write!(formatter, "semantic merge: {error}"),
            Self::Sqlite(error) => write!(formatter, "index: {error}"),
        }
    }
}

impl Error for PythonSemanticError {}

impl From<BatchError> for PythonSemanticError {
    fn from(error: BatchError) -> Self {
        Self::Backend(error)
    }
}
impl From<SemanticIndexError> for PythonSemanticError {
    fn from(error: SemanticIndexError) -> Self {
        Self::Index(error)
    }
}
impl From<MergeError> for PythonSemanticError {
    fn from(error: MergeError) -> Self {
        Self::Merge(error)
    }
}
impl From<rusqlite::Error> for PythonSemanticError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
impl From<GenerationError> for PythonSemanticError {
    fn from(error: GenerationError) -> Self {
        Self::Index(SemanticIndexError::from(error))
    }
}
impl From<crate::symbol::SymbolError> for PythonSemanticError {
    fn from(error: crate::symbol::SymbolError) -> Self {
        Self::Index(SemanticIndexError::from(error))
    }
}
impl From<crate::evidence::EvidenceError> for PythonSemanticError {
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
    pub policy: BatchPolicy,
}

/// What a refresh published and merged.
#[derive(Debug)]
pub struct RefreshOutcome {
    pub publication: SemanticPublication,
    pub merged: MergeOutcome,
    /// Gap kinds this backend does not answer, with the reason.
    pub deferred: Vec<adapter::DeferredGap>,
    /// Declarations that claim an override the evidence could not
    /// prove. An honest gap, never a guessed edge.
    pub unproven_overrides: Vec<UnprovenOverride>,
    pub evidence_count: usize,
    /// One line per piece of evidence: the capability, the site and
    /// what it resolved to. Diagnostic only -- it holds no source and
    /// no backend handle, and nothing reads it back.
    pub report: Vec<String>,
}

/// Resolve one Resource's semantic gaps, publish them as a candidate
/// generation, and merge them into the canonical graph.
///
/// The order matters and is not the obvious one. Evidence is collected
/// *before* the candidate generation opens, because the task 3 basis
/// has to name every Resource the analysis read and that set is only
/// known once the backend has answered. The window that opens is closed
/// by [`SemanticIndex::publish`], which revalidates the whole basis --
/// source revisions, config, environment, inventory and the Workspace
/// clock -- inside the publishing transaction. A Resource that moved
/// while the backend was thinking makes the candidate obsolete and
/// nothing is written.
pub fn refresh_resource(
    index: &SemanticIndex,
    queries: &dyn PythonQueries,
    request: &RefreshRequest<'_>,
) -> Result<RefreshOutcome, PythonSemanticError> {
    let connection = index.connection();
    let context_key = request.context.context_key();

    let owner = adapter::resource_by_id(connection, request.owner)?
        .ok_or(PythonSemanticError::UnknownResource(request.owner))?;
    let owner_text =
        fs::read_to_string(request.workspace_root.join(&owner.path_rel)).map_err(|error| {
            PythonSemanticError::UnreadableSource {
                path_rel: owner.path_rel.clone(),
                detail: error.to_string(),
            }
        })?;
    let occurrences = symbol::list_occurrences_for_resource(connection, owner.id)?;
    // Still-open gaps, plus the ones this context is already holding
    // closed -- otherwise a second refresh would withdraw its own work.
    let mut gaps = crate::evidence::list_unresolved_for_resource(connection, owner.id)?;
    for held in adapter::displaced_gaps(connection, &context_key, owner.id)? {
        if !gaps.iter().any(|open| open.occurrence == held.occurrence) {
            gaps.push(held);
        }
    }

    let profile = AnalysisProfile::semantic(request.context, request.capabilities);
    let analysis_profile_id = symbol::ensure_profile(connection, &profile)?;

    // One coherent snapshot for the whole Resource, restarted whole if
    // the backend invalidates it partway through.
    let owner_uri = protocol::path_to_uri(&request.workspace_root.join(&owner.path_rel));
    let own_symbols = symbol::list_for_resource(connection, owner.id)?;
    let mut collected: Option<(
        ResourceEvidence,
        BTreeSet<SymbolId>,
        BTreeMap<ResourceId, String>,
    )> = None;
    run_batch(queries, request.policy, &mut |batch: &Batch<'_>| {
        let paths = adapter::search_paths(batch, &owner_uri)?;
        let mut normalizer = Normalizer::new(connection, request.workspace_root, paths);
        let produced = resolve_resource(
            batch,
            &mut normalizer,
            &ResourceRequest {
                owner: &owner,
                owner_text: &owner_text,
                gaps: &gaps,
                occurrences: &occurrences,
                // Stamped once the candidate generation exists; merge
                // reads the grant's generation, never this field.
                generation_id: 0,
                analysis_profile_id,
                context_key: &context_key,
            },
        )?;
        // Resolved in the same snapshot as everything else, so a claim
        // and the facts around it describe one program state.
        let declared = adapter::declared_overrides(
            batch,
            &mut normalizer,
            &owner_uri,
            &owner_text,
            &own_symbols,
        )?;
        collected = Some((produced, declared, normalizer.sources_read().clone()));
        Ok(())
    })?;
    let (mut produced, declared_overrides, mut extra_sources) =
        collected.expect("a successful batch always records its result");

    // `OVERRIDES` is not written at a site, so no gap carries it: it is
    // derived from inheritance that is already proven, and anchored on
    // the overriding declaration's own Occurrence.
    let mut derived = overrides::derive(
        connection,
        &owner,
        &occurrences,
        &produced.resolved_bases,
        &context_key,
        &EvidenceBasis {
            owner_resource: owner.id,
            owner_resource_revision: owner.resource_revision.clone(),
            generation_id: 0,
            analysis_profile_id,
            resolution_context_key: None,
        },
        &declared_overrides,
    )
    .map_err(|error| {
        PythonSemanticError::Sqlite(rusqlite::Error::InvalidParameterName(error.to_string()))
    })?;
    // An ancestor's declaration is part of the proof, so the basis has
    // to name it: `Impl.run OVERRIDES Base.run` stops being true when
    // `base.py` moves, and task 3 is what notices.
    for ancestor in &derived.ancestor_resources {
        if let Some(resource) = adapter::resource_by_id(connection, *ancestor)? {
            extra_sources.insert(resource.id, resource.resource_revision);
        }
    }
    produced.evidence.append(&mut derived.evidence);

    let mut basis = SemanticBasis::new(request.context, request.config, owner.id)
        .with_source(owner.id, owner.resource_revision.clone());
    for (resource, revision) in extra_sources {
        basis = basis.with_source(resource, revision);
    }
    basis = basis.with_inventory(inventory_fingerprint(connection, request.context.language)?);

    let candidate = index.begin_candidate(basis, profile.clone(), Support::Supported)?;
    let generation_id = candidate.generation_id();
    for item in &mut produced.evidence {
        item.basis.generation_id = generation_id;
    }

    let current = CurrentInputs::new(request.context, request.config, request.capabilities)
        .with_inventory(inventory_fingerprint(connection, request.context.language)?);
    let publication = index.publish(candidate, &current)?;

    // Only a CURRENT publication may change canonical truth; merge
    // enforces it too, and this is where it becomes true.
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
                    crate::semantic::SemanticOutcome::Resolved { target } =>
                        format!("RESOLVED {:?}", target.entity_kind()),
                    crate::semantic::SemanticOutcome::Candidates { targets } =>
                        format!("CANDIDATES {}", targets.len()),
                    crate::semantic::SemanticOutcome::Unresolved { reason } =>
                        format!("UNRESOLVED {reason:?}"),
                }
            )
        })
        .collect();

    Ok(RefreshOutcome {
        publication,
        merged,
        evidence_count: produced.evidence.len(),
        deferred: produced.deferred,
        unproven_overrides: derived.unproven,
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
) -> Result<MergeOutcome, PythonSemanticError> {
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
            // A refused merge writes nothing at all.
            drop(transaction);
            generation::abort_generation(connection, building.id, &error.to_string())?;
            Err(PythonSemanticError::Merge(error))
        }
    }
}

#[cfg(test)]
pub(crate) mod tests_support;

#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod tests;
