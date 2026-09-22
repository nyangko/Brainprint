//! Turning Pyright answers into normalized Brainprint evidence.
//!
//! This is the boundary the whole backend exists behind:
//!
//! ```text
//! TSP/LSP URI · declaration node (UTF-16) · Type · resolveImport URI
//!         ↓  adapter
//! ResourceId · SymbolId · ExternalEntity · GraphEndpoint · byte SourceSpan
//! ```
//!
//! Nothing from the left-hand side survives the crossing. A URI is
//! resolved through the Workspace inventory, never turned into identity;
//! a range is converted exactly or the evidence is dropped; a type's
//! display name is not even decoded, because a name lookup is how a
//! same-name ambiguity becomes a confident wrong answer.
//!
//! What the adapter answers is exactly what I3 left open: it reads the
//! Resource's `unresolved_reference` rows and asks the backend about
//! each one, at the span I3 already recorded. Semantics is gap
//! resolution, not a second truth -- which is also why every piece of
//! evidence anchors to an Occurrence that already exists.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
};

use brainprint_core::{ResourceId, SymbolId};
use rusqlite::Connection;

use super::{
    coordinates::{LineMap, Range},
    protocol::{
        IncomingCall, Location, ModuleDescriptor, PythonRequest, PythonResponse, path_to_uri,
        uri_to_path,
    },
};
use crate::{
    evidence::OccurrenceRef,
    gaps::{IntendedRelation, PersistedUnresolved, UnresolvedReason},
    graph::{ExternalEntity, GraphEndpoint, RelationKind},
    resolution::{Dispatch, EvidenceBasis, Support},
    resource::Resource,
    runtime::{RequestFailure, RequestOptions, RuntimeLease, RuntimeRequest, SemanticRequestKey},
    semantic::{SemanticCapability, SemanticEvidence, SemanticOutcome},
    symbol::{Occurrence, OccurrenceKind},
};

/// The `ExternalEntity::kind` for a dependency module.
pub const EXTERNAL_MODULE: &str = "PYTHON_MODULE";
/// The `ExternalEntity::kind` for a name inside a dependency module.
pub const EXTERNAL_SYMBOL: &str = "PYTHON_SYMBOL";

// ---------------------------------------------------------------------
// Asking the backend
// ---------------------------------------------------------------------

/// Whatever can answer a typed Python question.
///
/// A trait so the normalization above it can be exercised against
/// scripted answers, without a Node process and without the workspace
/// test suite depending on a Pyright anyone happens to have installed.
pub trait PythonQueries {
    fn call(&self, request: &PythonRequest) -> Result<PythonResponse, RequestFailure>;
}

/// Asks through the task 2 supervisor, so timeout, cancellation,
/// dedupe and crash handling all stay where they already are.
pub struct LeaseQueries<'a> {
    lease: &'a RuntimeLease,
    options: RequestOptions,
}

impl<'a> LeaseQueries<'a> {
    #[must_use]
    pub const fn new(lease: &'a RuntimeLease, options: RequestOptions) -> Self {
        Self { lease, options }
    }
}

impl PythonQueries for LeaseQueries<'_> {
    fn call(&self, request: &PythonRequest) -> Result<PythonResponse, RequestFailure> {
        let payload = serde_json::to_vec(request).expect("a PythonRequest is always serializable");
        let key = SemanticRequestKey {
            context_key: self.lease.context_key().to_owned(),
            capability: capability_of(request),
            // The request itself is the question's identity, so two
            // callers asking it produce one backend round trip.
            target: String::from_utf8_lossy(&payload).into_owned(),
            // A snapshot is what makes two identical-looking questions
            // different questions, so it belongs in the dedupe key.
            basis_token: request.snapshot().map(|snapshot| snapshot.to_string()),
        };
        let response = self.lease.execute(
            RuntimeRequest {
                key,
                priority: crate::runtime::RequestPriority::Interactive,
                payload,
            },
            self.options,
        )?;
        serde_json::from_slice(&response.payload).map_err(|error| {
            RequestFailure::Backend(crate::runtime::HostError::new(format!(
                "undecodable Python response: {error}"
            )))
        })
    }
}

/// Which capability a question is asked under, for dedupe and telemetry.
fn capability_of(request: &PythonRequest) -> SemanticCapability {
    match request {
        PythonRequest::ResolveImport { .. } => SemanticCapability::ImportBinding,
        PythonRequest::DeclaredType { .. }
        | PythonRequest::ComputedType { .. }
        | PythonRequest::ExpectedType { .. } => SemanticCapability::TypeResolution,
        PythonRequest::Definition { .. }
        | PythonRequest::Declaration { .. }
        | PythonRequest::TypeDefinition { .. } => SemanticCapability::SymbolDefinition,
        PythonRequest::References { .. } => SemanticCapability::References,
        PythonRequest::PrepareCallHierarchy { .. } | PythonRequest::IncomingCalls { .. } => {
            SemanticCapability::CallsCrossFile
        }
        PythonRequest::ProtocolVersion
        | PythonRequest::Snapshot
        | PythonRequest::SearchPaths { .. }
        | PythonRequest::WatchedFilesChanged { .. } => SemanticCapability::ResourceDiscovery,
    }
}

// ---------------------------------------------------------------------
// Snapshot batches
// ---------------------------------------------------------------------

/// How many times a batch may be restarted when the snapshot moves.
///
/// Bounded, because task 5 observed the snapshot rotating throughout
/// the backend's initial analysis: an unbounded retry would spin for as
/// long as the project keeps changing, and never say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchPolicy {
    pub max_attempts: u32,
}

impl Default for BatchPolicy {
    fn default() -> Self {
        Self { max_attempts: 4 }
    }
}

/// Why a batch produced nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchError {
    /// The backend refused a query because its snapshot had moved. Only
    /// ever seen inside [`run_batch`], which restarts the whole batch.
    SnapshotStale,
    /// The snapshot kept moving. No evidence is returned: a partially
    /// collected batch is not a smaller answer, it is an incoherent one.
    RetriesExhausted {
        attempts: u32,
    },
    Request(RequestFailure),
    /// The backend answered a shape this adapter does not accept.
    Protocol(String),
}

impl fmt::Display for BatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SnapshotStale => formatter.write_str("the backend snapshot moved"),
            Self::RetriesExhausted { attempts } => write!(
                formatter,
                "the backend snapshot moved on every one of {attempts} attempts"
            ),
            Self::Request(failure) => write!(formatter, "{failure}"),
            Self::Protocol(detail) => write!(formatter, "unexpected backend answer: {detail}"),
        }
    }
}

impl Error for BatchError {}

impl From<RequestFailure> for BatchError {
    fn from(failure: RequestFailure) -> Self {
        Self::Request(failure)
    }
}

/// Every query in one batch, carried on one snapshot.
pub struct Batch<'a> {
    queries: &'a dyn PythonQueries,
    snapshot: u64,
}

impl Batch<'_> {
    #[must_use]
    pub const fn snapshot(&self) -> u64 {
        self.snapshot
    }

    /// Ask one question on this batch's snapshot.
    ///
    /// The snapshot is written into the request here rather than
    /// trusted from the caller, which is what makes "one candidate, one
    /// coherent snapshot" structural instead of a convention.
    pub fn call(&self, request: &PythonRequest) -> Result<PythonResponse, BatchError> {
        let request = with_snapshot(request, self.snapshot);
        match self.queries.call(&request)? {
            PythonResponse::SnapshotStale => Err(BatchError::SnapshotStale),
            answer => Ok(answer),
        }
    }
}

fn with_snapshot(request: &PythonRequest, snapshot: u64) -> PythonRequest {
    let mut request = request.clone();
    match &mut request {
        PythonRequest::SearchPaths { snapshot: slot, .. }
        | PythonRequest::ResolveImport { snapshot: slot, .. }
        | PythonRequest::DeclaredType { snapshot: slot, .. }
        | PythonRequest::ComputedType { snapshot: slot, .. }
        | PythonRequest::ExpectedType { snapshot: slot, .. } => *slot = snapshot,
        _ => {}
    }
    request
}

/// Run `body` against one coherent snapshot, restarting it whole if the
/// backend invalidates that snapshot partway through.
///
/// Restarting *whole* is the point. Half a batch from snapshot N and
/// half from N+1 is evidence about a program state that never existed,
/// and task 3 would happily publish it because each half validates.
pub fn run_batch<T>(
    queries: &dyn PythonQueries,
    policy: BatchPolicy,
    body: &mut dyn FnMut(&Batch<'_>) -> Result<T, BatchError>,
) -> Result<T, BatchError> {
    let attempts = policy.max_attempts.max(1);
    for _ in 0..attempts {
        let PythonResponse::Snapshot(snapshot) = queries.call(&PythonRequest::Snapshot)? else {
            return Err(BatchError::Protocol("expected a snapshot".to_owned()));
        };
        match body(&Batch { queries, snapshot }) {
            Err(BatchError::SnapshotStale) => {}
            other => return other,
        }
    }
    Err(BatchError::RetriesExhausted { attempts })
}

// ---------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------

/// What a backend location became.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Endpoint(GraphEndpoint),
    /// The location is real but cannot be named in Brainprint terms
    /// without guessing. Kept as a reason, never dropped.
    Unmappable(UnresolvedReason),
}

/// Resolves backend locations against the current Workspace.
pub struct Normalizer<'a> {
    connection: &'a Connection,
    workspace_root: &'a Path,
    /// Import roots, from `typeServer/getPythonSearchPaths`. Used to
    /// give a dependency file a module identity without reading it.
    search_paths: Vec<PathBuf>,
    /// Source text of Resources already read, so one batch reads each
    /// file once.
    texts: BTreeMap<ResourceId, String>,
    /// Every Resource whose current bytes a resolution depended on.
    read: BTreeMap<ResourceId, String>,
}

impl<'a> Normalizer<'a> {
    #[must_use]
    pub fn new(
        connection: &'a Connection,
        workspace_root: &'a Path,
        search_paths: Vec<String>,
    ) -> Self {
        let mut search_paths: Vec<PathBuf> = search_paths
            .iter()
            .filter_map(|uri| uri_to_path(uri))
            .collect();
        // Longest first, so a nested root wins over the one containing
        // it and `stdlib/json/__init__.pyi` becomes `json`, not
        // `stdlib.json`.
        search_paths.sort_by_key(|path| std::cmp::Reverse(path.as_os_str().len()));
        Self {
            connection,
            workspace_root,
            search_paths,
            texts: BTreeMap::new(),
            read: BTreeMap::new(),
        }
    }

    /// Every Resource read while resolving, with the revision it was
    /// read at. These belong in the task 3 basis: the answer depended
    /// on them, so it goes stale when they move.
    #[must_use]
    pub fn sources_read(&self) -> &BTreeMap<ResourceId, String> {
        &self.read
    }

    /// The active Workspace Resource a backend URI names, if any.
    fn resource_for_uri(&self, uri: &str) -> Result<Option<Resource>, BatchError> {
        let Some(path) = uri_to_path(uri) else {
            return Ok(None);
        };
        let Ok(relative) = path.strip_prefix(self.workspace_root) else {
            return Ok(None);
        };
        let path_key = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        resource_by_path_key(self.connection, &path_key).map_err(sqlite)
    }

    fn text_of(&mut self, resource: &Resource) -> Option<&str> {
        if !self.texts.contains_key(&resource.id) {
            let text = fs::read_to_string(self.workspace_root.join(&resource.path_rel)).ok()?;
            self.texts.insert(resource.id, text);
            self.read
                .insert(resource.id, resource.resource_revision.clone());
        }
        self.texts.get(&resource.id).map(String::as_str)
    }

    /// Turn one backend location into a Brainprint endpoint.
    ///
    /// `written_name` is the name the source writes at the referring
    /// site. It is used only to name an *external* symbol, so that a
    /// dependency target keeps the precision the site had without any
    /// dependency source being read or indexed.
    fn target_for(
        &mut self,
        location: &Location,
        written_name: Option<&str>,
    ) -> Result<Target, BatchError> {
        let Some(resource) = self.resource_for_uri(&location.uri)? else {
            return Ok(self.external_target(&location.uri, written_name));
        };
        // A whole-file range is the backend naming a module, not a
        // declaration inside one.
        if location.range.is_empty() {
            return Ok(Target::Endpoint(GraphEndpoint::Resource(resource.id)));
        }
        let Some(text) = self.text_of(&resource) else {
            return Ok(Target::Unmappable(UnresolvedReason::NoStructuralBinding));
        };
        let Ok((start, end)) = LineMap::new(text).span(location.range) else {
            // A range that does not convert exactly is refused rather
            // than clamped: the nearest span is a different symbol.
            return Ok(Target::Unmappable(UnresolvedReason::NoStructuralBinding));
        };
        match symbol_at_span(self.connection, resource.id, start, end).map_err(sqlite)? {
            SymbolMatch::One(symbol) => Ok(Target::Endpoint(GraphEndpoint::Symbol(symbol))),
            // Two Brainprint Symbols could be meant and nothing in the
            // answer chooses. Ordering is not evidence.
            SymbolMatch::Ambiguous => Ok(Target::Unmappable(UnresolvedReason::AmbiguousCandidates)),
            SymbolMatch::None => Ok(Target::Unmappable(UnresolvedReason::NoStructuralBinding)),
        }
    }

    fn external_target(&self, uri: &str, written_name: Option<&str>) -> Target {
        let Some(path) = uri_to_path(uri) else {
            return Target::Unmappable(UnresolvedReason::NoStructuralBinding);
        };
        match external_entity(&path, &self.search_paths, written_name) {
            Some(entity) => Target::Endpoint(GraphEndpoint::External(entity)),
            // Outside the Workspace and outside every import root:
            // there is no stable package identity to give it.
            None => Target::Unmappable(UnresolvedReason::NoStructuralBinding),
        }
    }
}

fn sqlite(error: rusqlite::Error) -> BatchError {
    BatchError::Protocol(error.to_string())
}

/// Give a dependency file a stable module identity from the import
/// roots the backend reported.
///
/// No dependency source is read and no Resource row is created: a
/// package, a dotted module path and, when the referring site wrote
/// one, a symbol name. That is enough to point at the same thing every
/// time without mirroring `site-packages` into the index.
#[must_use]
pub fn external_entity(
    path: &Path,
    search_paths: &[PathBuf],
    written_name: Option<&str>,
) -> Option<ExternalEntity> {
    let relative = search_paths
        .iter()
        .find_map(|root| path.strip_prefix(root).ok())?;
    let mut parts: Vec<String> = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    let last = parts.pop()?;
    // `json/__init__.pyi` and `json.py` are both the module `json`, so
    // the stub and the implementation collapse to one identity.
    let stem = last
        .rsplit_once('.')
        .map_or(last.as_str(), |(stem, _)| stem);
    if stem != "__init__" {
        parts.push(stem.to_owned());
    }
    if parts.is_empty() {
        return None;
    }
    let module_path = parts.join(".");
    let package_identity = parts[0].clone();
    // The written name's last segment: `json.dumps` names `dumps`.
    let symbol_name = written_name
        .and_then(|name| name.rsplit('.').next())
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    Some(ExternalEntity {
        package_identity,
        qualified_name: symbol_name
            .as_ref()
            .map(|name| format!("{module_path}.{name}")),
        kind: if symbol_name.is_some() {
            EXTERNAL_SYMBOL.to_owned()
        } else {
            EXTERNAL_MODULE.to_owned()
        },
        module_path: Some(module_path),
        symbol_name,
        // Task 8 owns dependency versions, and a locator would be a
        // machine path in canonical identity.
        resolved_version: None,
        declaration_locator: None,
    })
}

use crate::semantic_normalize::{SymbolMatch, resource_by_path_key, symbol_at_span};
/// The Workspace lookups shared with every other semantic adapter.
/// Same index, same exact-span rule, one implementation.
pub use crate::semantic_normalize::{displaced_gaps, resource_by_id};

// ---------------------------------------------------------------------
// Resolving one Resource's gaps
// ---------------------------------------------------------------------

/// A gap this task deliberately leaves alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredGap {
    pub occurrence: OccurrenceRef,
    pub intended: IntendedRelation,
    pub reason: &'static str,
}

/// Task 6 resolves exactly the relation kinds it declares a capability
/// for. `EXTENDS`, `IMPLEMENTS`, `OVERRIDES` and an unknown-kind
/// inheritance entry all need derivation the backend does not offer --
/// #19 task 5 measured `textDocument/implementation` and
/// `prepareTypeHierarchy` answering `MethodNotFound` -- so they stay
/// open for task 7 rather than being guessed here.
const DEFERRED_TO_TASK_SEVEN: &str = "python override/inheritance enrichment is #19 task 7";

/// What one Resource's semantic pass produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceEvidence {
    pub evidence: Vec<SemanticEvidence>,
    pub deferred: Vec<DeferredGap>,
    /// `(subclass, base)` pairs this pass established, so override
    /// derivation can see a base that is not in the canonical graph
    /// yet -- the merge for this refresh has not run.
    pub resolved_bases: Vec<(SymbolId, SymbolId)>,
}

/// One Resource's inputs for a semantic pass.
pub struct ResourceRequest<'a> {
    pub owner: &'a Resource,
    /// The owner's current bytes, as the Workspace has them.
    pub owner_text: &'a str,
    pub gaps: &'a [PersistedUnresolved],
    pub occurrences: &'a [Occurrence],
    pub generation_id: i64,
    pub analysis_profile_id: i64,
    pub context_key: &'a str,
}

/// Ask the backend about every gap this Resource owns, and normalize
/// the answers.
pub fn resolve_resource(
    batch: &Batch<'_>,
    normalizer: &mut Normalizer<'_>,
    request: &ResourceRequest<'_>,
) -> Result<ResourceEvidence, BatchError> {
    let owner_uri = path_to_uri(&normalizer.workspace_root.join(&request.owner.path_rel));
    let map = LineMap::new(request.owner_text);
    let mut produced = ResourceEvidence::default();

    for gap in request.gaps {
        let Some(resolvable) = ResolvableKind::of(gap.intended) else {
            produced.deferred.push(deferred(gap));
            continue;
        };
        let kind = resolvable.relation_kind();

        let outcome = resolve_one(
            batch,
            normalizer,
            &map,
            &owner_uri,
            gap,
            resolvable,
            request.owner_text,
        )?;

        let source = source_endpoint(request.owner.id, request.occurrences, gap.occurrence);
        if resolvable == ResolvableKind::Extends
            && let SemanticOutcome::Resolved {
                target: GraphEndpoint::Symbol(base),
            } = &outcome
            && let GraphEndpoint::Symbol(subclass) = source
        {
            // Remembered so override derivation in this same pass can
            // use a base this pass has only just established. The
            // subclass is the Occurrence's containing Symbol -- the
            // same ownership the canonical edge carries.
            produced.resolved_bases.push((subclass, *base));
        }

        produced.evidence.push(SemanticEvidence {
            context_key: request.context_key.to_owned(),
            capability: resolvable.capability(&outcome),
            relation_kind: Some(kind),
            basis: EvidenceBasis {
                owner_resource: request.owner.id,
                owner_resource_revision: request.owner.resource_revision.clone(),
                generation_id: request.generation_id,
                analysis_profile_id: request.analysis_profile_id,
                resolution_context_key: None,
            },
            occurrence: Some(gap.occurrence),
            source: Some(source),
            outcome,
            support: Support::Supported,
            dispatch: resolvable.dispatch(gap),
        });
    }
    Ok(produced)
}

fn deferred(gap: &PersistedUnresolved) -> DeferredGap {
    DeferredGap {
        occurrence: gap.occurrence,
        intended: gap.intended,
        reason: DEFERRED_TO_TASK_SEVEN,
    }
}

/// The gap kinds the adapter resolves from a source occurrence.
///
/// Each one is a site I3 already recorded and left open, so the answer
/// has somewhere exact to anchor. `IMPLEMENTS` is absent because Python
/// has no implements clause, and `OVERRIDES` is absent because it is
/// not written at a site at all -- it is derived (see
/// [`super::overrides`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvableKind {
    Imports,
    Calls,
    References,
    UsesType,
    /// A base-list entry. In Python a base list is always inheritance:
    /// the language has no implements clause, and `class X(P)` against
    /// a Protocol is still ordinary subclassing.
    Extends,
}

impl ResolvableKind {
    const fn of(intended: IntendedRelation) -> Option<Self> {
        match intended {
            IntendedRelation::Known(RelationKind::Imports) => Some(Self::Imports),
            IntendedRelation::Known(RelationKind::Calls) => Some(Self::Calls),
            IntendedRelation::Known(RelationKind::References) => Some(Self::References),
            IntendedRelation::Known(RelationKind::UsesType) => Some(Self::UsesType),
            // A base whose class/interface distinction structural
            // resolution could not make is still a base, because Python
            // has only one kind of base.
            IntendedRelation::Known(RelationKind::Extends) | IntendedRelation::Inheritance => {
                Some(Self::Extends)
            }
            _ => None,
        }
    }

    /// The canonical relation this gap becomes once it resolves.
    const fn relation_kind(self) -> RelationKind {
        match self {
            Self::Imports => RelationKind::Imports,
            Self::Calls => RelationKind::Calls,
            Self::References => RelationKind::References,
            Self::UsesType => RelationKind::UsesType,
            Self::Extends => RelationKind::Extends,
        }
    }

    /// Which capability answered, which depends on where the target
    /// turned out to be.
    fn capability(self, outcome: &SemanticOutcome) -> SemanticCapability {
        let external = matches!(
            outcome,
            SemanticOutcome::Resolved {
                target: GraphEndpoint::External(_)
            }
        );
        match self {
            Self::Imports if external => SemanticCapability::ExternalSymbolResolution,
            Self::Imports => SemanticCapability::ImportBinding,
            Self::Calls if external => SemanticCapability::CallsCrossFile,
            Self::Calls => SemanticCapability::CallsIntraFile,
            Self::References => SemanticCapability::References,
            Self::UsesType => SemanticCapability::TypeResolution,
            Self::Extends => SemanticCapability::Inheritance,
        }
    }

    /// A member call binds through the receiver's declared type, and a
    /// subclass may still take over at run time. Saying `STATIC` there
    /// would claim more than the backend proved.
    fn dispatch(self, gap: &PersistedUnresolved) -> Dispatch {
        match self {
            Self::Calls if gap.module_hint.is_some() => Dispatch::Unknown,
            _ => Dispatch::Static,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_one(
    batch: &Batch<'_>,
    normalizer: &mut Normalizer<'_>,
    map: &LineMap<'_>,
    owner_uri: &str,
    gap: &PersistedUnresolved,
    kind: ResolvableKind,
    owner_text: &str,
) -> Result<SemanticOutcome, BatchError> {
    let span = (gap.occurrence.start_byte, gap.occurrence.end_byte);
    let written = owner_text.get(span.0..span.1).unwrap_or_default();

    let targets = match kind {
        ResolvableKind::UsesType => {
            let Ok(range) = map.range(span.0, span.1) else {
                return Ok(unconvertible());
            };
            type_targets(batch, normalizer, owner_uri, range, written)?
        }
        // A module path is resolved as a module. A bare name is not:
        // `import json` and `from pkg import json` write the same text,
        // and only the definition answer tells them apart.
        ResolvableKind::Imports if ModuleDescriptor::parse_module_site(written).is_some() => {
            let module = ModuleDescriptor::parse_module_site(written).expect("just checked");
            match batch.call(&PythonRequest::ResolveImport {
                source_uri: owner_uri.to_owned(),
                module,
                snapshot: batch.snapshot(),
            })? {
                PythonResponse::Import(Some(uri)) => vec![normalizer.target_for(
                    &Location {
                        uri,
                        range: Range::new(
                            super::coordinates::Position::new(0, 0),
                            super::coordinates::Position::new(0, 0),
                        ),
                    },
                    None,
                )?],
                PythonResponse::Import(None) => Vec::new(),
                PythonResponse::Unsupported(method) => {
                    return Err(BatchError::Protocol(format!("{method} is unsupported")));
                }
                other => return Err(BatchError::Protocol(format!("{other:?}"))),
            }
        }
        ResolvableKind::Imports
        | ResolvableKind::Calls
        | ResolvableKind::References
        | ResolvableKind::Extends => {
            // The *last* character of the span. I3 records a call site
            // at the whole callee, so asking at the start of `x.run`
            // answers about the receiver `x` -- a real, wrong target.
            let Ok(position) = map.last_character_position(span.0, span.1) else {
                return Ok(unconvertible());
            };
            match batch.call(&PythonRequest::Definition {
                uri: owner_uri.to_owned(),
                position,
            })? {
                PythonResponse::Locations(locations) => {
                    let name = (!gap.lookup_name.is_empty()).then_some(gap.lookup_name.as_str());
                    let mut targets = Vec::new();
                    for location in &locations {
                        targets.push(normalizer.target_for(location, name)?);
                    }
                    targets
                }
                PythonResponse::Unsupported(method) => {
                    return Err(BatchError::Protocol(format!("{method} is unsupported")));
                }
                other => return Err(BatchError::Protocol(format!("{other:?}"))),
            }
        }
    };

    Ok(outcome_of(targets, gap))
}

fn type_targets(
    batch: &Batch<'_>,
    normalizer: &mut Normalizer<'_>,
    owner_uri: &str,
    range: Range,
    _written: &str,
) -> Result<Vec<Target>, BatchError> {
    for request in [
        PythonRequest::DeclaredType {
            uri: owner_uri.to_owned(),
            range,
            snapshot: batch.snapshot(),
        },
        PythonRequest::ComputedType {
            uri: owner_uri.to_owned(),
            range,
            snapshot: batch.snapshot(),
        },
    ] {
        let PythonResponse::Type(answer) = batch.call(&request)? else {
            continue;
        };
        let Some(answer) = answer else { continue };
        if let Some(declaration) = &answer.declaration {
            return Ok(vec![normalizer.target_for(declaration, None)?]);
        }
        if let Some(uri) = &answer.module_uri {
            return Ok(vec![normalizer.target_for(
                &Location {
                    uri: uri.clone(),
                    range: Range::new(
                        super::coordinates::Position::new(0, 0),
                        super::coordinates::Position::new(0, 0),
                    ),
                },
                None,
            )?]);
        }
        // A type with a display name and no declaration is not a
        // target. Looking the name up would be the guess this tier
        // exists to refuse.
    }
    Ok(Vec::new())
}

fn outcome_of(targets: Vec<Target>, gap: &PersistedUnresolved) -> SemanticOutcome {
    let mut endpoints: Vec<GraphEndpoint> = Vec::new();
    let mut unmappable: Option<UnresolvedReason> = None;
    for target in targets {
        match target {
            Target::Endpoint(endpoint) => {
                // A stub and its implementation normalize to the same
                // external module, so two answers become one target.
                if !endpoints.contains(&endpoint) {
                    endpoints.push(endpoint);
                }
            }
            Target::Unmappable(reason) => unmappable = unmappable.or(Some(reason)),
        }
    }
    match endpoints.len() {
        1 => SemanticOutcome::Resolved {
            target: endpoints.remove(0),
        },
        0 => SemanticOutcome::Unresolved {
            // Keeping I3's reason when the backend added nothing is
            // more honest than replacing it with a new one.
            reason: unmappable.unwrap_or(gap.reason),
        },
        // Several distinct targets: the backend did not settle it, so
        // neither does this.
        _ => SemanticOutcome::Candidates { targets: endpoints },
    }
}

/// A span that does not convert exactly is never asked about: the
/// nearest position is a different token, and the backend would answer
/// about that one.
const fn unconvertible() -> SemanticOutcome {
    SemanticOutcome::Unresolved {
        reason: UnresolvedReason::NoStructuralBinding,
    }
}

/// The endpoint a site is *from*: the Symbol that lexically contains
/// it, or the Resource at file level. The same rule I3 uses, so
/// structural and semantic evidence for one edge agree on its source.
fn source_endpoint(
    owner: ResourceId,
    occurrences: &[Occurrence],
    site: OccurrenceRef,
) -> GraphEndpoint {
    occurrences
        .iter()
        .find(|occurrence| {
            occurrence.kind == site.kind
                && occurrence.span.start_byte == site.start_byte
                && occurrence.span.end_byte == site.end_byte
        })
        .and_then(|occurrence| occurrence.containing_symbol_id)
        .map_or(GraphEndpoint::Resource(owner), GraphEndpoint::Symbol)
}

// ---------------------------------------------------------------------
// Call hierarchy
// ---------------------------------------------------------------------

/// Turn `callHierarchy/incomingCalls` into CALLS evidence for one
/// Resource.
///
/// Direct protocol-to-evidence only: each caller's `fromRanges` are
/// exact call-site ranges, and a range is kept only when it converts
/// exactly *and* an existing `CALL_SITE` Occurrence already sits there.
/// Nothing is derived, and no Occurrence is invented -- task 4 refuses
/// those for the same reason.
pub fn normalize_incoming_calls(
    calls: &[IncomingCall],
    target: &GraphEndpoint,
    request: &ResourceRequest<'_>,
    owner_uri: &str,
) -> Vec<SemanticEvidence> {
    let map = LineMap::new(request.owner_text);
    let mut evidence = Vec::new();
    for call in calls {
        if call.from.uri != owner_uri {
            // Another Resource's call sites belong to that Resource's
            // own contribution: task 4 merges one owner at a time.
            continue;
        }
        for range in &call.from_ranges {
            let Ok((start, end)) = map.span(*range) else {
                continue;
            };
            let site = OccurrenceRef {
                kind: OccurrenceKind::CallSite,
                start_byte: start,
                end_byte: end,
            };
            let anchored = request.occurrences.iter().any(|occurrence| {
                occurrence.kind == OccurrenceKind::CallSite
                    && occurrence.span.start_byte == start
                    && occurrence.span.end_byte == end
            });
            if !anchored {
                continue;
            }
            let item = SemanticEvidence {
                context_key: request.context_key.to_owned(),
                capability: SemanticCapability::CallsCrossFile,
                relation_kind: Some(RelationKind::Calls),
                basis: EvidenceBasis {
                    owner_resource: request.owner.id,
                    owner_resource_revision: request.owner.resource_revision.clone(),
                    generation_id: request.generation_id,
                    analysis_profile_id: request.analysis_profile_id,
                    resolution_context_key: None,
                },
                occurrence: Some(site),
                source: Some(source_endpoint(request.owner.id, request.occurrences, site)),
                outcome: SemanticOutcome::Resolved {
                    target: target.clone(),
                },
                support: Support::Supported,
                dispatch: Dispatch::Unknown,
            };
            if !evidence.contains(&item) {
                evidence.push(item);
            }
        }
    }
    evidence
}

/// Which members of `owner` carry a decorator that really is
/// `typing.override`.
///
/// The decorator is found syntactically and then *resolved*: the name
/// has to land in `typing` or `typing_extensions`, so a local function
/// called `override` does not count. It is validation evidence only --
/// it never creates an edge, and a claim whose ancestor member cannot
/// be proven is reported as an honest gap instead.
pub fn declared_overrides(
    batch: &Batch<'_>,
    normalizer: &mut Normalizer<'_>,
    owner_uri: &str,
    owner_text: &str,
    symbols: &[crate::symbol::Symbol],
) -> Result<BTreeSet<SymbolId>, BatchError> {
    let map = LineMap::new(owner_text);
    let mut declared = BTreeSet::new();
    for site in super::overrides::override_decorator_sites(owner_text, symbols) {
        let Ok(position) = map.last_character_position(site.start_byte, site.end_byte) else {
            continue;
        };
        let PythonResponse::Locations(locations) = batch.call(&PythonRequest::Definition {
            uri: owner_uri.to_owned(),
            position,
        })?
        else {
            continue;
        };
        for location in &locations {
            if let Target::Endpoint(GraphEndpoint::External(entity)) =
                normalizer.target_for(location, None)?
                && entity
                    .module_path
                    .as_deref()
                    .is_some_and(|module| super::overrides::OVERRIDE_MODULES.contains(&module))
            {
                declared.insert(site.member);
            }
        }
    }
    Ok(declared)
}

/// Read the import roots the backend uses, for external identity.
pub fn search_paths(batch: &Batch<'_>, from_uri: &str) -> Result<Vec<String>, BatchError> {
    match batch.call(&PythonRequest::SearchPaths {
        from_uri: from_uri.to_owned(),
        snapshot: batch.snapshot(),
    })? {
        PythonResponse::SearchPaths(paths) => Ok(paths),
        PythonResponse::Unsupported(_) => Ok(Vec::new()),
        other => Err(BatchError::Protocol(format!("{other:?}"))),
    }
}

/// Tell the backend that files changed on disk.
///
/// The only synchronization Brainprint performs. When it fires is task
/// 8's; that this is the channel is task 5's decision.
pub fn notify_watched_files(
    queries: &dyn PythonQueries,
    changes: Vec<super::protocol::WatchedChange>,
) -> Result<(), RequestFailure> {
    queries
        .call(&PythonRequest::WatchedFilesChanged { changes })
        .map(|_| ())
}
