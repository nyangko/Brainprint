//! Turning Svelte language server answers into normalized Brainprint
//! evidence.
//!
//! ```text
//! LSP URI · UTF-16 range in the ORIGINAL .svelte file
//!         ↓  adapter
//! ResourceId · SymbolId · ExternalEntity · GraphEndpoint · byte SourceSpan
//! ```
//!
//! ## The mapping this tier does *not* do
//!
//! The obvious design for Svelte is: generate `svelte2tsx` output, ask
//! TypeScript about it, consume the source map, translate every answer
//! back. #19 task 11 measured whether that is necessary and it is not --
//! the official server already answers in original `.svelte`
//! coordinates, for template identifiers, component tags, script types
//! and cross-file targets alike. So Brainprint consumes that boundary,
//! which is also the boundary the task told it to prefer.
//!
//! What remains is to make that a *checked* fact rather than a trusted
//! one, and that is [`Normalizer::target_for`]'s job. Three refusals,
//! none of which can be satisfied by approximation:
//!
//! * A URI that names generated output is refused outright
//!   ([`is_generated_uri`]). Nothing `__sveltets_*` can become a span.
//! * A URI that names no ACTIVE Workspace Resource and no dependency
//!   package is refused. A generated file living somewhere this build
//!   does not recognise fails here even if the marker check misses it.
//! * A range that does not convert to an exact byte span is refused. The
//!   nearest span is a different declaration, and an approximate answer
//!   about source an Agent will edit is worse than no answer.
//!
//! Every refusal becomes an explicit gap, which task 4 turns into
//! coverage rather than into silence.
//!
//! ## Component identity
//!
//! Measured, not chosen: asked for the definition of `<Child …>`, the
//! server answers `src/lib/Child.svelte` at a *degenerate* range
//! (`0:1-0:1`). That is the backend saying "this component is this
//! file". So the canonical target is
//! [`GraphEndpoint::Resource`] for the `.svelte` Resource -- not a
//! synthetic class Symbol with borrowed coordinates, which is what
//! naming the `svelte2tsx` class would have meant.

use std::{collections::BTreeMap, error::Error, fmt, fs, path::Path};

use brainprint_core::ResourceId;
use rusqlite::Connection;

use super::protocol::{
    Location, POSITION_ENCODING, SvelteRequest, SvelteResponse, is_generated_uri, path_to_uri,
    uri_to_path,
};
use crate::{
    gaps::UnresolvedReason,
    graph::{ExternalEntity, GraphEndpoint},
    lsp::coordinates::LineMap,
    resolution::{Dispatch, EvidenceBasis, Support},
    resource::Resource,
    runtime::{RequestFailure, RequestOptions, RuntimeLease, RuntimeRequest, SemanticRequestKey},
    semantic::{SemanticCapability, SemanticEvidence, SemanticOutcome},
    semantic_normalize::{SymbolMatch, resource_by_path_key, symbol_at_span},
    symbol::Occurrence,
};

/// The Workspace lookups and site vocabulary shared with every other
/// adapter. Same index, same rules, one implementation.
pub use crate::semantic_normalize::{
    DeferredGap, ResolvableKind, RetainedExternal, SemanticSite, SiteSet, collect_sites,
    displaced_gaps, resource_by_id, source_endpoint,
};

/// The `ExternalEntity::kind` for a dependency package.
pub const EXTERNAL_PACKAGE: &str = "NODE_PACKAGE";
/// The `ExternalEntity::kind` for a name declared inside one.
pub const EXTERNAL_SYMBOL: &str = "NODE_SYMBOL";

const NODE_MODULES: &str = "node_modules";

// ---------------------------------------------------------------------
// Asking the backend
// ---------------------------------------------------------------------

/// Whatever can answer a typed Svelte question.
pub trait SvelteQueries {
    /// # Errors
    /// When the backend could not be reached or refused the request.
    fn call(&self, request: &SvelteRequest) -> Result<SvelteResponse, RequestFailure>;
}

/// Asks through the task 2 supervisor, so timeout, cancellation, dedupe
/// and crash handling all stay where they already are.
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

impl SvelteQueries for LeaseQueries<'_> {
    fn call(&self, request: &SvelteRequest) -> Result<SvelteResponse, RequestFailure> {
        let payload = serde_json::to_vec(request).expect("a SvelteRequest is always serializable");
        let key = SemanticRequestKey {
            context_key: self.lease.context_key().to_owned(),
            capability: match request {
                SvelteRequest::Definition { .. } => SemanticCapability::SymbolDefinition,
                SvelteRequest::WatchedFilesChanged { .. } => SemanticCapability::ResourceDiscovery,
            },
            target: String::from_utf8_lossy(&payload).into_owned(),
            // No snapshot token exists on this backend either. Task 2
            // dedupes only requests in flight together, so an absent
            // basis is correct rather than convenient.
            basis_token: None,
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
                "undecodable Svelte response: {error}"
            )))
        })
    }
}

// ---------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------

/// Why a normalization pass produced nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterError {
    Request(RequestFailure),
    /// The backend answered a shape this adapter does not accept, or a
    /// method it does not implement.
    Protocol(String),
    /// A component was asked about before it was synchronized. This tier
    /// owns the ordering, so it is a bug here rather than a fact about
    /// the component -- and it is loud instead of an empty answer.
    Unsynchronized {
        uri: String,
    },
}

impl fmt::Display for AdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(failure) => write!(formatter, "{failure}"),
            Self::Protocol(detail) => write!(formatter, "unexpected backend answer: {detail}"),
            Self::Unsynchronized { uri } => {
                write!(formatter, "{uri} was queried before it was synchronized")
            }
        }
    }
}

impl Error for AdapterError {}

impl From<RequestFailure> for AdapterError {
    fn from(failure: RequestFailure) -> Self {
        Self::Request(failure)
    }
}

fn sqlite(error: rusqlite::Error) -> AdapterError {
    AdapterError::Protocol(error.to_string())
}

// ---------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------

/// Why a location could not become Brainprint identity.
///
/// Kept apart from the generic "no binding" reason so a report can tell
/// a genuine gap from a mapping this tier refused, which is what
/// [`MappingFailure`] counts are for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingFailure {
    /// The answer named `svelte2tsx` output.
    GeneratedLocation,
    /// The answer named a file that is not an indexed Resource and not a
    /// dependency package.
    OutsideWorkspace,
    /// The range did not convert to an exact byte span.
    InexactRange,
    /// The span is inside a Resource but names no single Symbol.
    NoSymbolAtSpan,
}

/// What a backend location became.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Endpoint(GraphEndpoint),
    Unmappable(MappingFailure),
}

/// Resolves backend locations against the current Workspace.
pub struct Normalizer<'a> {
    connection: &'a Connection,
    workspace_root: &'a Path,
    texts: BTreeMap<ResourceId, String>,
    read: BTreeMap<ResourceId, String>,
    failures: Vec<(MappingFailure, String)>,
}

impl<'a> Normalizer<'a> {
    #[must_use]
    pub fn new(connection: &'a Connection, workspace_root: &'a Path) -> Self {
        Self {
            connection,
            workspace_root,
            texts: BTreeMap::new(),
            read: BTreeMap::new(),
            failures: Vec::new(),
        }
    }

    /// Every Resource read while resolving, with the revision it was
    /// read at. These belong in the task 3 basis.
    #[must_use]
    pub const fn sources_read(&self) -> &BTreeMap<ResourceId, String> {
        &self.read
    }

    /// Every location this pass refused, and why.
    ///
    /// Reported rather than dropped: a mapping that could not be made is
    /// incomplete coverage, and the difference between "no answer" and
    /// "an answer I would not publish" is the whole contract.
    #[must_use]
    pub fn mapping_failures(&self) -> &[(MappingFailure, String)] {
        &self.failures
    }

    /// The active Workspace Resource a backend URI names, if any.
    fn resource_for_uri(&self, uri: &str) -> Result<Option<Resource>, AdapterError> {
        let Some(path) = uri_to_path(uri) else {
            return Ok(None);
        };
        let Ok(relative) = path.strip_prefix(self.workspace_root) else {
            return Ok(None);
        };
        if relative
            .components()
            .any(|component| component.as_os_str() == NODE_MODULES)
        {
            return Ok(None);
        }
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

    fn refuse(&mut self, failure: MappingFailure, uri: &str) -> Target {
        self.failures.push((failure, uri.to_owned()));
        Target::Unmappable(failure)
    }

    /// Turn one backend location into a Brainprint endpoint.
    fn target_for(
        &mut self,
        location: &Location,
        written_name: Option<&str>,
    ) -> Result<Target, AdapterError> {
        // The hard gate. A generated position is not source anyone
        // wrote, so it may not become a span an Agent is handed.
        if is_generated_uri(&location.uri) {
            return Ok(self.refuse(MappingFailure::GeneratedLocation, &location.uri));
        }
        let Some(resource) = self.resource_for_uri(&location.uri)? else {
            return Ok(self.external_target(&location.uri, written_name));
        };
        // A degenerate range is the backend naming a *component*, not a
        // declaration inside one. Measured: `<Child />` answers
        // `Child.svelte` at `0:1-0:1`.
        if location.range.is_empty() {
            // Its *bytes* were not read, but the proof still depends on
            // it: "`<Child>` names this component" stops being true when
            // that component moves. So it enters the task 3 basis, which
            // is what makes a dependent go stale with it.
            self.read
                .insert(resource.id, resource.resource_revision.clone());
            return Ok(Target::Endpoint(GraphEndpoint::Resource(resource.id)));
        }
        let Some(text) = self.text_of(&resource) else {
            return Ok(self.refuse(MappingFailure::OutsideWorkspace, &location.uri));
        };
        let Ok((start, end)) = LineMap::with_encoding(text, POSITION_ENCODING).span(location.range)
        else {
            return Ok(self.refuse(MappingFailure::InexactRange, &location.uri));
        };
        match symbol_at_span(self.connection, resource.id, start, end).map_err(sqlite)? {
            SymbolMatch::One(symbol) => Ok(Target::Endpoint(GraphEndpoint::Symbol(symbol))),
            SymbolMatch::Ambiguous | SymbolMatch::None => {
                Ok(self.refuse(MappingFailure::NoSymbolAtSpan, &location.uri))
            }
        }
    }

    fn external_target(&mut self, uri: &str, written_name: Option<&str>) -> Target {
        let Some(path) = uri_to_path(uri) else {
            return self.refuse(MappingFailure::OutsideWorkspace, uri);
        };
        match external_entity(&path, written_name) {
            Some(entity) => Target::Endpoint(GraphEndpoint::External(entity)),
            None => self.refuse(MappingFailure::OutsideWorkspace, uri),
        }
    }
}

/// Give a dependency declaration a stable package identity.
///
/// The same rule the TypeScript tier uses -- the path after the last
/// `node_modules` segment -- because it is the same package layout. No
/// dependency source is read and no Resource row is created.
#[must_use]
pub fn external_entity(path: &Path, written_name: Option<&str>) -> Option<ExternalEntity> {
    let parts: Vec<String> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    let last = parts.iter().rposition(|part| part == NODE_MODULES)?;
    let mut inside = parts[last + 1..].iter();
    let first = inside.next()?.clone();
    let package_identity = if first.starts_with('@') {
        format!("{first}/{}", inside.next()?)
    } else {
        first
    };
    let mut module_parts: Vec<String> = inside.cloned().collect();
    if let Some(file) = module_parts.pop() {
        let stem = strip_module_extension(&file);
        if stem != "index" && !stem.is_empty() {
            module_parts.push(stem.to_owned());
        }
    }
    let module_path = (!module_parts.is_empty()).then(|| module_parts.join("/"));
    let symbol_name = written_name
        .and_then(|name| name.rsplit('.').next())
        .filter(|name| {
            !name.is_empty()
                && name.chars().all(|character| {
                    character.is_alphanumeric() || character == '_' || character == '$'
                })
        })
        .map(str::to_owned);
    Some(ExternalEntity {
        package_identity,
        qualified_name: symbol_name.as_ref().map(|name| match &module_path {
            Some(module) => format!("{module}.{name}"),
            None => name.clone(),
        }),
        kind: if symbol_name.is_some() {
            EXTERNAL_SYMBOL.to_owned()
        } else {
            EXTERNAL_PACKAGE.to_owned()
        },
        module_path,
        symbol_name,
        resolved_version: None,
        declaration_locator: None,
    })
}

fn strip_module_extension(file: &str) -> &str {
    for suffix in [
        ".d.ts", ".d.mts", ".d.cts", ".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs",
        ".svelte",
    ] {
        if let Some(stem) = file.strip_suffix(suffix) {
            return stem;
        }
    }
    file
}

// ---------------------------------------------------------------------
// Resolving one component
// ---------------------------------------------------------------------

/// What one component's semantic pass produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceEvidence {
    pub evidence: Vec<SemanticEvidence>,
    pub retained_external: Vec<RetainedExternal>,
    pub deferred: Vec<DeferredGap>,
    /// Locations this pass refused to publish, and why.
    pub mapping_failures: Vec<(MappingFailure, String)>,
}

/// One component's inputs for a semantic pass.
pub struct ResourceRequest<'a> {
    pub owner: &'a Resource,
    /// The owner's current bytes, as the Workspace has them.
    pub owner_text: &'a str,
    pub sites: &'a SiteSet,
    pub occurrences: &'a [Occurrence],
    pub generation_id: i64,
    pub analysis_profile_id: i64,
    pub context_key: &'a str,
}

/// How this backend reads a shared [`ResolvableKind`].
trait SvelteSite {
    fn capability(self, outcome: &SemanticOutcome, site: &SemanticSite) -> SemanticCapability;
    fn dispatch(self) -> Dispatch;
}

impl SvelteSite for ResolvableKind {
    fn capability(self, outcome: &SemanticOutcome, site: &SemanticSite) -> SemanticCapability {
        let external = matches!(
            outcome,
            SemanticOutcome::Resolved {
                target: GraphEndpoint::External(_)
            }
        );
        match self {
            _ if external => SemanticCapability::ExternalSymbolResolution,
            Self::Imports if site.specifier => SemanticCapability::ImportBinding,
            Self::Imports => SemanticCapability::AliasResolution,
            Self::Calls => SemanticCapability::CallsIntraFile,
            // A template use crossing from markup into the script is the
            // capability this tier exists for, and it is the one that
            // proves the original-source mapping.
            Self::References => SemanticCapability::References,
            Self::UsesType => SemanticCapability::TypeResolution,
            Self::Extends | Self::Implements => SemanticCapability::Inheritance,
        }
    }

    /// Always static. A Svelte template writes a binding, not a virtual
    /// call, and this tier claims no dispatch analysis at all.
    fn dispatch(self) -> Dispatch {
        Dispatch::Static
    }
}

/// Ask the backend about every site this component offers, and normalize
/// the answers.
///
/// # Errors
/// When the backend cannot be reached or answers a shape this build does
/// not read.
pub fn resolve_resource(
    queries: &dyn SvelteQueries,
    normalizer: &mut Normalizer<'_>,
    request: &ResourceRequest<'_>,
) -> Result<ResourceEvidence, AdapterError> {
    let owner_uri = path_to_uri(&normalizer.workspace_root.join(&request.owner.path_rel));
    let map = LineMap::with_encoding(request.owner_text, POSITION_ENCODING);
    let mut produced = ResourceEvidence {
        retained_external: request.sites.retained_external.clone(),
        deferred: request.sites.deferred.clone(),
        ..ResourceEvidence::default()
    };

    for site in &request.sites.sites {
        // A module specifier is asked about *inside* the quotes; a name
        // at its last character.
        let position = if site.specifier {
            map.position(site.occurrence.start_byte + 1)
        } else {
            map.last_character_position(site.occurrence.start_byte, site.occurrence.end_byte)
        };
        let Ok(position) = position else {
            produced.evidence.push(unresolved(request, site));
            continue;
        };

        let locations = match queries.call(&SvelteRequest::Definition {
            uri: owner_uri.clone(),
            position,
        })? {
            SvelteResponse::Locations(locations) => locations,
            SvelteResponse::Unsupported(method) => {
                return Err(AdapterError::Protocol(format!("{method} is unsupported")));
            }
            SvelteResponse::Unsynchronized(uri) => {
                return Err(AdapterError::Unsynchronized { uri });
            }
            other => return Err(AdapterError::Protocol(format!("{other:?}"))),
        };

        let name = (!site.lookup_name.is_empty() && !site.specifier).then_some(&site.lookup_name);
        let mut endpoints: Vec<GraphEndpoint> = Vec::new();
        let mut refused: Option<MappingFailure> = None;
        for location in &locations {
            match normalizer.target_for(location, name.map(String::as_str))? {
                Target::Endpoint(endpoint) => {
                    if !endpoints.contains(&endpoint) {
                        endpoints.push(endpoint);
                    }
                }
                Target::Unmappable(failure) => refused = refused.or(Some(failure)),
            }
        }
        // The server answering "here" is not a fact: a definition that
        // lands on the site it was asked about is the binding naming
        // itself, and an edge from a site to itself says nothing.
        let source = source_endpoint(request.owner.id, request.occurrences, site.occurrence);
        endpoints.retain(|endpoint| !is_self_answer(endpoint, request.owner.id, site, &source));

        let outcome = match endpoints.len() {
            1 => SemanticOutcome::Resolved {
                target: endpoints.remove(0),
            },
            0 => SemanticOutcome::Unresolved {
                reason: refused.map_or(site.reason, mapping_reason),
            },
            _ => SemanticOutcome::Candidates { targets: endpoints },
        };

        produced.evidence.push(SemanticEvidence {
            context_key: request.context_key.to_owned(),
            capability: site.kind.capability(&outcome, site),
            relation_kind: Some(site.kind.relation_kind()),
            basis: EvidenceBasis {
                owner_resource: request.owner.id,
                owner_resource_revision: request.owner.resource_revision.clone(),
                generation_id: request.generation_id,
                analysis_profile_id: request.analysis_profile_id,
                resolution_context_key: None,
            },
            occurrence: Some(site.occurrence),
            source: Some(source),
            outcome,
            support: Support::Supported,
            dispatch: site.kind.dispatch(),
        });
    }

    produced.mapping_failures = normalizer.mapping_failures().to_vec();
    Ok(produced)
}

/// Whether a target is the site asking the question.
///
/// Two shapes. A Symbol answer equal to the source endpoint -- a local
/// binding naming itself. And a Resource answer naming the owner, which
/// happens when a component references something in its own file at a
/// degenerate position.
fn is_self_answer(
    endpoint: &GraphEndpoint,
    owner: ResourceId,
    _site: &SemanticSite,
    source: &GraphEndpoint,
) -> bool {
    match endpoint {
        GraphEndpoint::Resource(resource) => *resource == owner,
        GraphEndpoint::Symbol(_) => endpoint == source,
        _ => false,
    }
}

const fn mapping_reason(failure: MappingFailure) -> UnresolvedReason {
    match failure {
        // The backend answered, and this tier refused to publish where
        // it pointed. That is a gap in *coverage*, and saying so keeps
        // it apart from "nothing declares this name".
        MappingFailure::GeneratedLocation
        | MappingFailure::OutsideWorkspace
        | MappingFailure::InexactRange
        | MappingFailure::NoSymbolAtSpan => UnresolvedReason::NoStructuralBinding,
    }
}

fn unresolved(request: &ResourceRequest<'_>, site: &SemanticSite) -> SemanticEvidence {
    SemanticEvidence {
        context_key: request.context_key.to_owned(),
        capability: SemanticCapability::SymbolDefinition,
        relation_kind: Some(site.kind.relation_kind()),
        basis: EvidenceBasis {
            owner_resource: request.owner.id,
            owner_resource_revision: request.owner.resource_revision.clone(),
            generation_id: request.generation_id,
            analysis_profile_id: request.analysis_profile_id,
            resolution_context_key: None,
        },
        occurrence: Some(site.occurrence),
        source: Some(source_endpoint(
            request.owner.id,
            request.occurrences,
            site.occurrence,
        )),
        outcome: SemanticOutcome::Unresolved {
            reason: site.reason,
        },
        support: Support::Supported,
        dispatch: Dispatch::Static,
    }
}

// ---------------------------------------------------------------------
// Filesystem synchronization
// ---------------------------------------------------------------------

/// Tell the backend that files changed on disk.
///
/// The only synchronization this tier performs, and the barrier the
/// whole freshness model rests on. It is also what makes the *first*
/// question about a component answerable at all: the measured server
/// errors on a document it has never been told about, which is why
/// nothing here can silently return zero.
///
/// # Errors
/// When the notification could not be delivered.
pub fn notify_watched_files(
    queries: &dyn SvelteQueries,
    changes: Vec<super::protocol::WatchedChange>,
) -> Result<(), RequestFailure> {
    queries
        .call(&SvelteRequest::WatchedFilesChanged { changes })
        .map(|_| ())
}
