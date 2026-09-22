//! The TypeScript / JavaScript semantic backend.
//!
//! One `tsc --lsp --stdio` child process -- the TypeScript 7 native
//! language server -- answers for TypeScript, JavaScript, TSX and JSX
//! alike. There is no second backend, no second graph, and no
//! `typescript_*` query surface: after the task 4 merge the existing
//! `callers`, `references`, impact and prepared-inspection APIs are what
//! answer, enriched rather than duplicated.
//!
//! The layering:
//!
//! ```text
//! SemanticRuntimeSupervisor            (#19 task 2, backend-neutral)
//!         ↓ SemanticBackendLauncher
//! TypeScriptLauncher → TypeScriptHost  (launcher, host)
//!         ↓ TypeScriptRequest / TypeScriptResponse
//! typescript-go LSP server             (protocol)
//!         ↓ adapter
//! SemanticEvidence                     (#19 task 1)
//!         ↓ SemanticIndex candidate    (#19 task 3)
//!         ↓ merge                      (#19 task 4)
//! the one canonical Brainprint graph
//! ```
//!
//! [`crate::lsp`] carries the framing and the coordinate mapping, shared
//! with the Python backend rather than reimplemented -- but nothing
//! TypeScript-specific went into it. Method names, the compatibility
//! class, the project model and the dependency-environment policy all
//! live here, because they are what this server means and not what LSP
//! means.
//!
//! ## What the transport probe settled
//!
//! The design issue named a `tsserver` family, written while TypeScript
//! was mid-port. #19 task 10 opened by measuring the artifact instead of
//! trusting that, and Candidate A -- the native LSP -- answered every
//! P0 capability, so Candidate B was never built. See [`protocol`] for
//! the recorded version, command line and handshake, and
//! [`WATCHER_DECISION`] for the filesystem-synchronization measurement.

pub mod adapter;
pub mod host;
pub mod launcher;
pub mod lifecycle;
pub mod protocol;

/// `OVERRIDES` derivation, shared with the Python tier: both languages
/// derive the relation from proven `EXTENDS` edges and declared members,
/// and only the syntax of an override *claim* differs.
pub use crate::semantic_overrides as overrides;

use std::{fmt, fs, path::Path};

use brainprint_core::ResourceId;
use rusqlite::Connection;

pub use adapter::{
    AdapterError, LeaseQueries, Normalizer, ResolvableKind, ResourceEvidence, ResourceRequest,
    SemanticSite, SiteSet, TypeScriptQueries,
};
pub use host::{TypeScriptHost, TypeScriptSettings};
pub use launcher::{InstallError, TypeScriptInstall, TypeScriptLauncher};
pub use lifecycle::{
    BackendReadiness, ChangeKind, ChangePlan, ConfigLimit, ConfigSource, EnvironmentAssurance,
    EnvironmentIdentity, LifecycleError, OwnerOutcome, ResourceChange, SemanticAvailability,
    TypeScriptProjectConfig,
};
pub use overrides::{UnprovenOverride, UnprovenReason};
pub use protocol::{COMPATIBILITY_CLASS, ProtocolCompatibility, TESTED_BACKEND_VERSION};

use crate::{
    generation::{self, GenerationError},
    merge::{self, MergeError, MergeOutcome, MergeRequest},
    resolution::{EvidenceBasis, Support},
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

/// How Brainprint tells this backend that the Workspace moved.
///
/// #19 task 10 required this be measured rather than copied from the
/// Python backend, and specifically flagged reports of a native-LSP
/// `Created` notification leaving stale module state. It was measured,
/// on 7.0.2, three ways over the same fixture -- a new export added to
/// an existing module and imported, a brand-new module created and
/// imported, and a module deleted:
///
/// ```text
/// server-side watching (client declares no didChangeWatchedFiles)  PASS
/// client-side workspace/didChangeWatchedFiles                      PASS
/// textDocument/didOpen + didChange overlay                         PASS
/// ```
///
/// All three were correct, and all three stayed correct with *no*
/// settle delay between the change and the query. The reported stale
/// export state did not reproduce on this version.
///
/// Client-side watched-file notifications win anyway, for a reason the
/// pass/fail table does not show. The other two each fail a requirement
/// the task states outright:
///
/// * A document overlay would make an editor buffer a second reality
///   alongside the Workspace filesystem. It also bought nothing: the
///   server already reads the same bytes Brainprint indexed.
/// * The server's own watcher works, but it is an OS watcher on a
///   timeline Brainprint cannot observe. There is no moment at which
///   Brainprint *knows* the backend has seen a change, so publishing
///   currentness would be a guess that happened to be right on a local
///   APFS volume and need not be on a network mount or another platform.
///
/// A notification gives what neither does: JSON-RPC over one ordered
/// stdio connection delivers it strictly before any request sent after
/// it, so "the backend has caught up" is a fact about message order
/// rather than about elapsed time. That ordering is this backend's
/// synchronization barrier, and it is why there is no snapshot number
/// here -- see [`FRESHNESS_MODEL`].
pub const WATCHER_DECISION: &str =
    "client-side workspace/didChangeWatchedFiles, no document overlay";

/// This backend's in-flight freshness currency, or rather the absence of
/// one.
///
/// The Python backend carries a `typeServer/getSnapshot` number and
/// retries a batch the server declares stale. The TypeScript native LSP
/// exposes no equivalent: no project version, no result id, and no
/// server-side staleness rejection. The task is explicit that a snapshot
/// number must not be invented when the backend does not expose one, so
/// none is.
///
/// What replaces it is the ordering barrier in [`WATCHER_DECISION`]: a
/// request issued after a watched-file notification on the same
/// connection is answered against the changed filesystem. Nothing
/// process-local is persisted either way -- task 3 freshness stays
/// Resource revisions, config fingerprint, environment fingerprint,
/// inventory fingerprint and analysis profile, exactly as before.
pub const FRESHNESS_MODEL: &str = "connection ordering; the backend exposes no snapshot token";

/// How well overload resolution is covered.
///
/// SUPPORTED, and measured rather than assumed. This was the task's
/// named design-stop risk: if the public LSP boundary could not prove
/// which overload a call site selected, the alternative was compiler
/// internals, and the instruction was to stop and report the conflict
/// rather than reach for them.
///
/// It did not come to that. On the fixture's
/// `parse(value: string): StringResult` / `parse(value: number):
/// NumberResult` pair, `textDocument/definition` at `parse("a")`
/// answered the *string* overload's declaration line and at `parse(1)`
/// the *number* overload's -- two different targets from the same name,
/// chosen by argument type, from the public boundary. `textDocument/
/// hover` corroborated each with the selected signature, and
/// `signatureHelp` returned the whole declaration set with the server's
/// own active index.
///
/// So the three things the task asks Brainprint to distinguish all come
/// from the server: the overload *declaration set* from `signatureHelp`,
/// the *call-site selection* from `definition`, and the *implementation*
/// declaration as the one signature `definition` never selects. None of
/// it is decided by matching parameter text here.
pub const OVERLOAD_SUPPORT: Support = Support::Supported;

/// How well `IMPLEMENTS` is covered.
///
/// SUPPORTED, and the clearest difference from the Python tier. #19
/// task 5 measured `textDocument/implementation` answering
/// `MethodNotFound` on Pyright, which is why Python had to *derive*
/// inheritance in task 7 with its own evidence. The TypeScript server
/// declares `implementationProvider` and answers: asked at the `Runner`
/// interface the fixture declares, it returned both implementing
/// classes across two files.
pub const IMPLEMENTS_SUPPORT: Support = Support::Supported;

/// How well a call site's bound target is covered.
///
/// A direct call binds to a declaration and is recorded `STATIC`. A call
/// through a typed receiver binds to the *declaration the receiver's
/// type names*, which in a language with subclassing is not necessarily
/// the code that runs, so it is recorded `UNKNOWN` rather than claimed
/// as a static target. Both answers are exact; only one of them is the
/// capability, hence PARTIAL.
///
/// The measurement that makes this worth stating: the fixture declares
/// `Alpha`, `Beta` and `Gamma`, each with an unrelated `run()`, and a
/// function taking a `Beta`. `b.run()` resolved to `Beta.run` and to
/// nothing else. That is a real type-directed answer, not a name match
/// -- and it is still a declaration, not a dispatch target.
pub const CALL_TARGET_SUPPORT: Support = Support::Partial;

/// How well JavaScript type resolution is covered.
///
/// PARTIAL, and deliberately not rounded up because the server returns
/// something for every position. Measured on the fixture's `js/`
/// directory: a JSDoc `@param {string}` does produce a real signature
/// (`function measure(name: string): number`), and a `@type` annotation
/// a real object shape. But an unannotated parameter hovers as `any`,
/// and `obj[name]()` yields nothing a relation could anchor to.
///
/// Those are honest gaps and stay gaps. The rule the task states and
/// this tier follows: a false zero is unacceptable, an explicit
/// PARTIAL is not.
pub const JAVASCRIPT_TYPE_SUPPORT: Support = Support::Partial;

// ---------------------------------------------------------------------
// Capability report
// ---------------------------------------------------------------------

/// How well overload resolution comes through the *canonical graph*.
///
/// Not the same claim as [`OVERLOAD_SUPPORT`], which is about the
/// transport. That one says the public LSP boundary can tell the
/// overloads apart; this one says a call site's canonical `CALLS` edge
/// points at the declaration the server selected, which is what a
/// caller actually reads.
pub const OVERLOAD_GRAPH_SUPPORT: Support = Support::Supported;

/// How well a TypeScript type annotation is covered.
///
/// PARTIAL, and the reason is structural rather than semantic. I3
/// records a `TYPE_SITE` only for a *plain* type name -- a generic
/// application (`Box<T>`), a union, a qualified path and a keyword all
/// produce no Occurrence at all, deliberately, because their target is
/// not in the syntax. The backend can name the declaration for every
/// one of them, and this tier still cannot publish it: task 4 refuses
/// evidence with no anchor, and manufacturing an Occurrence to hold an
/// edge is the one thing it must never do.
///
/// So a generic *declaration* resolves -- `import type { Box }` binds
/// to the interface, and that is how a generic type target is proven --
/// while a generic *application* at an annotation site stays a gap.
/// Rounding this up to SUPPORTED would report those sites as "no type
/// here".
pub const TYPE_ANNOTATION_SUPPORT: Support = Support::Partial;

/// How well JavaScript module semantics are covered.
///
/// SUPPORTED for the forms the fixture proves: ESM `import`/`export`,
/// CommonJS `require`, `module.exports` and `exports.name`. A dynamic
/// `require(name)` or `obj[key]()` is not covered and is not claimed --
/// the backend proves nothing there, so neither does this.
pub const JAVASCRIPT_MODULE_SUPPORT: Support = Support::Supported;

/// What the TypeScript backend actually answers, as implemented here.
///
/// Declared from this tier's measured behaviour over the acceptance
/// fixture, not from the server's theoretical surface: `typescript-go`
/// can do more than the adapter wires up, and claiming the difference
/// would be a coverage claim with no code behind it.
///
/// The Resource and Structure capabilities are deliberately absent.
/// They are I2/I3's, they are already answered without any backend, and
/// listing them here would let a language server take credit for the
/// parser's work -- and, worse, make them look unavailable when the
/// backend is.
#[must_use]
pub fn capability_report(context: &AnalysisContext) -> CapabilityReport {
    let mut report = CapabilityReport::new(context);
    report
        // A module specifier the structural tier left open resolves to
        // its Resource; `./public.js` reaching `public.ts` is the
        // extension-rewrite case TypeScript requires and syntax cannot
        // settle.
        .declare(SemanticCapability::ImportBinding, Support::Supported)
        // `import { Service } from "@core/service"` reaches the class
        // through a path alias, and `import { PublicModel }` reaches
        // `Model` through a rename. Both measured, neither by name.
        .declare(SemanticCapability::AliasResolution, Support::Supported)
        // `export { Model as PublicModel } from "./model.js"` and
        // `export * from "./core/types.js"`. I3 records no Occurrence
        // on a re-export line, so the adapter chases the hop rather
        // than reading the export syntax itself.
        .declare(SemanticCapability::ReexportResolution, Support::Supported)
        .declare(
            SemanticCapability::ExternalSymbolResolution,
            Support::Supported,
        )
        .declare(SemanticCapability::SymbolDefinition, Support::Supported)
        .declare(SemanticCapability::References, Support::Supported)
        .declare(SemanticCapability::CallsIntraFile, Support::Supported)
        .declare(SemanticCapability::CallsCrossFile, Support::Supported)
        // See [`CALL_TARGET_SUPPORT`]: a target is always exact, a
        // *static* target is only claimed where it is one.
        .declare(
            SemanticCapability::StaticDispatchTarget,
            CALL_TARGET_SUPPORT,
        )
        .declare(SemanticCapability::TypeResolution, TYPE_ANNOTATION_SUPPORT)
        // `class Child extends Base` resolves wherever I3 anchored the
        // heritage clause, which for TypeScript is every plain base
        // name -- the language allows only one base and writes it as an
        // identifier.
        .declare(SemanticCapability::Inheritance, Support::Supported)
        // The clearest difference from the Python tier: TypeScript
        // writes an `implements` clause, I3 records it, and the backend
        // resolves it. Nothing has to be derived from member shape.
        .declare(SemanticCapability::Implements, Support::Supported)
        .declare(
            SemanticCapability::OverloadResolution,
            OVERLOAD_GRAPH_SUPPORT,
        )
        // Derived from proven `EXTENDS`, exactly as Python's is. PARTIAL
        // because an ancestor outside the Workspace has no indexed
        // members, so "nothing declares this name" stops being a
        // reliable answer and the site is reported unproven instead.
        .declare(SemanticCapability::Overrides, Support::Partial)
        // The backend answers `textDocument/implementation` -- #19 task
        // 10 measured it returning both implementing classes for
        // `Runner` -- and the canonical graph answers the same question
        // from the `IMPLEMENTS` and `OVERRIDES` edges this tier
        // publishes. PARTIAL because the adapter reaches it through
        // those edges rather than by issuing the query, so a member an
        // `implements` clause does not name is not reached.
        .declare(SemanticCapability::ImplementationTarget, Support::Partial);
    report
}

/// What the *JavaScript* half answers.
///
/// A separate report, not an inference from the TypeScript one. Same
/// process, same adapter, same fixture run against `js/` instead of
/// `src/` -- and a different answer, because the language carries less.
/// Forcing symmetry here would be claiming type facts that a `.js` file
/// does not state.
#[must_use]
pub fn javascript_capability_report(context: &AnalysisContext) -> CapabilityReport {
    let mut report = CapabilityReport::new(context);
    report
        .declare(SemanticCapability::ImportBinding, JAVASCRIPT_MODULE_SUPPORT)
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
        // A direct call binds; `obj[name]()` does not, and the backend
        // proves nothing there. PARTIAL for the same reason TypeScript's
        // is, plus that one.
        .declare(SemanticCapability::StaticDispatchTarget, Support::Partial)
        // UNSUPPORTED *through the canonical graph*, and not because
        // the backend cannot answer: [`JAVASCRIPT_TYPE_SUPPORT`]
        // measured a JSDoc `@param {string}` producing a real
        // signature. I3 records a `TYPE_SITE` only where the syntax
        // states a type, and in JavaScript the types live in comments,
        // so there is no Occurrence for a type fact to anchor to and
        // task 4 rightly refuses an unanchored one. Declaring this
        // PARTIAL would promise edges no query would ever find.
        .declare(SemanticCapability::TypeResolution, Support::Unsupported)
        // Same shape, same reason. `class JsChild extends JsBase` is
        // real inheritance the backend resolves, and I3's heritage
        // evidence is written against the TypeScript dialect's
        // `extends_clause` node, which the JavaScript grammar does not
        // produce -- so no `TYPE_SITE` exists to anchor it. An I3
        // change, not this tier's, and claimed as UNSUPPORTED until it
        // happens rather than as a capability nothing delivers.
        .declare(SemanticCapability::Inheritance, Support::Unsupported)
        // JavaScript has no `implements` clause. Inferring conformance
        // from matching members is duck typing, not evidence.
        .declare(SemanticCapability::Implements, Support::Unsupported)
        // No overload declarations exist in JavaScript.
        .declare(SemanticCapability::OverloadResolution, Support::Unsupported)
        // Derived from proven `EXTENDS`, which JavaScript does not
        // produce here -- so the derivation runs and finds nothing.
        .declare(SemanticCapability::Overrides, Support::Unsupported)
        .declare(
            SemanticCapability::ImplementationTarget,
            Support::Unsupported,
        );
    report
}

// ---------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------

/// The toolchain a TypeScript/JavaScript semantic result depends on.
///
/// The install directory is deliberately absent: where the server lives
/// does not change what TypeScript code means, and a machine path in
/// identity would make the same project a different context on every
/// machine. What *is* in it is the package-resolution environment
/// ([`lifecycle::environment_identity`]), because the same project
/// resolves imports differently after an install.
///
/// Every field is known before anything is started, which it has to be:
/// the [`AnalysisContext`] built from it is what *selects* the runtime,
/// so it cannot depend on a running one. The negotiated server version
/// is therefore not an input but a gate -- [`TypeScriptLauncher`]
/// refuses to start against a server outside
/// [`protocol::COMPATIBILITY_CLASS`], so a publication under this
/// identity can only ever have come from a protocol the adapter has
/// read.
#[must_use]
pub fn toolchain_identity(
    install: &launcher::TypeScriptInstall,
    environment: &lifecycle::EnvironmentIdentity,
) -> ToolchainIdentity {
    ToolchainIdentity {
        backend_version: install.manifest_version.clone(),
        backend_compatibility_class: protocol::COMPATIBILITY_CLASS.to_owned(),
        environment_fingerprint: environment.fingerprint.clone(),
    }
}

// ---------------------------------------------------------------------
// Refresh
// ---------------------------------------------------------------------

/// Why a refresh produced nothing.
#[derive(Debug)]
pub enum TypeScriptSemanticError {
    /// The Resource is not in the current Workspace inventory.
    UnknownResource(ResourceId),
    /// Its bytes could not be read, so no span could be converted.
    UnreadableSource {
        path_rel: String,
        detail: String,
    },
    Backend(adapter::AdapterError),
    Index(SemanticIndexError),
    Merge(MergeError),
    Sqlite(rusqlite::Error),
}

impl fmt::Display for TypeScriptSemanticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownResource(resource) => {
                write!(formatter, "{resource} is not an active Resource")
            }
            Self::UnreadableSource { path_rel, detail } => {
                write!(formatter, "could not read {path_rel}: {detail}")
            }
            Self::Backend(error) => write!(formatter, "typescript semantic backend: {error}"),
            Self::Index(error) => write!(formatter, "semantic publication: {error}"),
            Self::Merge(error) => write!(formatter, "semantic merge: {error}"),
            Self::Sqlite(error) => write!(formatter, "index: {error}"),
        }
    }
}

impl std::error::Error for TypeScriptSemanticError {}

impl From<adapter::AdapterError> for TypeScriptSemanticError {
    fn from(error: adapter::AdapterError) -> Self {
        Self::Backend(error)
    }
}
impl From<SemanticIndexError> for TypeScriptSemanticError {
    fn from(error: SemanticIndexError) -> Self {
        Self::Index(error)
    }
}
impl From<MergeError> for TypeScriptSemanticError {
    fn from(error: MergeError) -> Self {
        Self::Merge(error)
    }
}
impl From<rusqlite::Error> for TypeScriptSemanticError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
impl From<GenerationError> for TypeScriptSemanticError {
    fn from(error: GenerationError) -> Self {
        Self::Index(SemanticIndexError::from(error))
    }
}
impl From<crate::symbol::SymbolError> for TypeScriptSemanticError {
    fn from(error: crate::symbol::SymbolError) -> Self {
        Self::Index(SemanticIndexError::from(error))
    }
}
impl From<crate::evidence::EvidenceError> for TypeScriptSemanticError {
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
    /// The encoding the live connection negotiated. Passed in rather
    /// than assumed, because reading a UTF-8 range as UTF-16 puts every
    /// span at a plausible wrong offset.
    pub encoding: protocol::PositionEncodingChoice,
}

/// What a refresh published and merged.
#[derive(Debug)]
pub struct RefreshOutcome {
    pub publication: SemanticPublication,
    pub merged: MergeOutcome,
    /// Sites left with the structural tier's dependency classification.
    pub retained_external: Vec<adapter::RetainedExternal>,
    /// Sites this tier does not answer, with the reason.
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

/// Resolve one Resource's semantic sites, publish them as a candidate
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
///
/// # Errors
/// When the backend cannot answer, the publication is refused, or the
/// merge is.
pub fn refresh_resource(
    index: &SemanticIndex,
    queries: &dyn adapter::TypeScriptQueries,
    request: &RefreshRequest<'_>,
) -> Result<RefreshOutcome, TypeScriptSemanticError> {
    let connection = index.connection();
    let context_key = request.context.context_key();

    let owner = adapter::resource_by_id(connection, request.owner)?
        .ok_or(TypeScriptSemanticError::UnknownResource(request.owner))?;
    let owner_text =
        fs::read_to_string(request.workspace_root.join(&owner.path_rel)).map_err(|error| {
            TypeScriptSemanticError::UnreadableSource {
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
    let sites = adapter::collect_sites(connection, &owner, &owner_text, &gaps, &context_key)?;

    let profile = AnalysisProfile::semantic(request.context, request.capabilities);
    let analysis_profile_id = symbol::ensure_profile(connection, &profile)?;

    let own_symbols = symbol::list_for_resource(connection, owner.id)?;
    let mut normalizer =
        adapter::Normalizer::new(connection, request.workspace_root, request.encoding);
    let mut produced = adapter::resolve_resource(
        queries,
        &mut normalizer,
        &adapter::ResourceRequest {
            owner: &owner,
            owner_text: &owner_text,
            sites: &sites,
            occurrences: &occurrences,
            // Stamped once the candidate generation exists; merge reads
            // the grant's generation, never this field.
            generation_id: 0,
            analysis_profile_id,
            context_key: &context_key,
        },
    )?;
    let mut extra_sources = normalizer.sources_read().clone();

    // `OVERRIDES` is not written at a site, so no gap carries it: it is
    // derived from inheritance that is already proven, and anchored on
    // the overriding declaration's own Occurrence. The `override`
    // keyword is a *claim*, never a source of edges -- it only decides
    // whether an unprovable claim is worth reporting.
    let declared_overrides =
        adapter::override_keyword_members(&owner_text, &own_symbols, &occurrences);
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
        TypeScriptSemanticError::Sqlite(rusqlite::Error::InvalidParameterName(error.to_string()))
    })?;
    // An ancestor's declaration is part of the proof, so the basis has
    // to name it: `Child.run OVERRIDES Base.run` stops being true when
    // the file declaring `Base` moves, and task 3 is what notices.
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
    basis = basis.with_inventory(lifecycle::inventory_fingerprint(connection)?);

    let candidate = index.begin_candidate(basis, profile.clone(), Support::Supported)?;
    let generation_id = candidate.generation_id();
    for item in &mut produced.evidence {
        item.basis.generation_id = generation_id;
    }

    let current = CurrentInputs::new(request.context, request.config, request.capabilities)
        .with_inventory(lifecycle::inventory_fingerprint(connection)?);
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
        retained_external: produced.retained_external,
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
) -> Result<MergeOutcome, TypeScriptSemanticError> {
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
            Err(TypeScriptSemanticError::Merge(error))
        }
    }
}

#[cfg(test)]
pub(crate) mod tests_support;

#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod tests;
