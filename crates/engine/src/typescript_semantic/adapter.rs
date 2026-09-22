//! Turning TypeScript native LSP answers into normalized Brainprint
//! evidence.
//!
//! This is the boundary the whole backend exists behind:
//!
//! ```text
//! LSP URI · range in the negotiated encoding · hover prose · signature set
//!         ↓  adapter
//! ResourceId · SymbolId · ExternalEntity · GraphEndpoint · byte SourceSpan
//! ```
//!
//! Nothing from the left-hand side survives the crossing. A URI is
//! resolved through the Workspace inventory, never turned into
//! identity; a range is converted exactly or the evidence is dropped;
//! a displayed signature is never parsed to choose a target, because
//! matching parameter text is precisely how an overload set becomes a
//! confident wrong answer.
//!
//! ## What the adapter answers
//!
//! Every fact anchors to an Occurrence I3 already recorded, and the
//! work is bounded by that set -- the owner's import sites, call sites,
//! reference sites and type sites, and nothing else. The TypeScript
//! program is not enumerated into a second graph.
//!
//! Two of those sites are deliberately skipped:
//!
//! * A site the structural tier already bound to an **external**
//!   package. There the two tiers are not proving the same thing: I3
//!   names the *specifier* (`path`), and the backend names the
//!   *declaration file* (`@types/node/path.d.ts`). Emitting the second
//!   over the first would report every dependency import as a
//!   contradiction, and every path-aliased import too, since a bare
//!   `@core/service` is classified as a package by shape rather than by
//!   reading `tsconfig` path mapping. The structural classification
//!   stands, it is reported in [`ResourceEvidence::retained_external`],
//!   and the *bindings* that specifier introduces -- the imported names
//!   -- are what this tier proves instead. That is where a path alias
//!   becomes a Workspace Symbol.
//! * A type site whose relation kind nothing states: no gap, no bound
//!   relation. Task 4 refuses evidence with no relation kind, and
//!   guessing one from the syntax is I3's job, not this tier's.
//!
//! ## No snapshot, no batch
//!
//! The Python backend carries a snapshot number and restarts a whole
//! batch when it moves. This server exposes no such token
//! ([`super::FRESHNESS_MODEL`]), so there is nothing to carry and
//! nothing to retry. Coherence comes from the ordering barrier instead:
//! a request issued after a `workspace/didChangeWatchedFiles`
//! notification on the same stdio connection is answered against the
//! changed filesystem, and task 3 revalidates the whole basis inside
//! the publishing transaction anyway.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
};

use brainprint_core::{ResourceId, SymbolId};
use rusqlite::{Connection, params};

use super::protocol::{
    Location, PositionEncodingChoice, TypeScriptRequest, TypeScriptResponse, path_to_uri,
    uri_to_path,
};
use crate::{
    evidence::OccurrenceRef,
    gaps::{IntendedRelation, PersistedUnresolved, UnresolvedReason},
    graph::{ExternalEntity, GraphEndpoint, RelationKind},
    lsp::coordinates::LineMap,
    resolution::{Dispatch, EvidenceBasis, Support},
    resource::Resource,
    runtime::{RequestFailure, RequestOptions, RuntimeLease, RuntimeRequest, SemanticRequestKey},
    semantic::{SemanticCapability, SemanticEvidence, SemanticOutcome},
    semantic_normalize::{SymbolMatch, resource_by_path_key, symbol_at_span},
    symbol::{Occurrence, OccurrenceKind, Symbol, SymbolKind},
};

/// The Workspace lookups shared with every other semantic adapter.
pub use crate::semantic_normalize::{displaced_gaps, resource_by_id};

/// The `ExternalEntity::kind` for a dependency package.
pub const EXTERNAL_PACKAGE: &str = "NODE_PACKAGE";
/// The `ExternalEntity::kind` for a name declared inside one.
pub const EXTERNAL_SYMBOL: &str = "NODE_SYMBOL";

/// The directory name that makes a path a dependency path.
const NODE_MODULES: &str = "node_modules";

/// How many re-export hops a definition answer may be chased through.
///
/// `export { Model as PublicModel } from "./model.js"` is a Workspace
/// file that declares no Brainprint Symbol -- I3 records no Occurrence
/// for a re-export -- so a definition that lands there has to be asked
/// again to reach the declaration. Bounded, because a mid-edit or
/// circular barrel file must not become an unbounded walk.
const MAX_REEXPORT_HOPS: usize = 4;

// ---------------------------------------------------------------------
// Asking the backend
// ---------------------------------------------------------------------

/// Whatever can answer a typed TypeScript/JavaScript question.
///
/// A trait so the normalization above it can be exercised against
/// scripted answers, without a `tsc` process and without the workspace
/// test suite depending on a TypeScript anyone happens to have
/// installed.
pub trait TypeScriptQueries {
    /// # Errors
    /// When the backend could not be reached or refused the request.
    fn call(&self, request: &TypeScriptRequest) -> Result<TypeScriptResponse, RequestFailure>;
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

impl TypeScriptQueries for LeaseQueries<'_> {
    fn call(&self, request: &TypeScriptRequest) -> Result<TypeScriptResponse, RequestFailure> {
        let payload =
            serde_json::to_vec(request).expect("a TypeScriptRequest is always serializable");
        let key = SemanticRequestKey {
            context_key: self.lease.context_key().to_owned(),
            capability: capability_of(request),
            // The request itself is the question's identity, so two
            // callers asking it produce one backend round trip.
            target: String::from_utf8_lossy(&payload).into_owned(),
            // Deliberately absent. Task 2 dedupes only requests that
            // are in flight together, and this backend has no snapshot
            // token that could make two identical questions different
            // questions -- inventing one here would be inventing the
            // very thing `FRESHNESS_MODEL` says does not exist.
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
                "undecodable TypeScript response: {error}"
            )))
        })
    }
}

/// Which capability a question is asked under, for dedupe and
/// telemetry.
fn capability_of(request: &TypeScriptRequest) -> SemanticCapability {
    match request {
        TypeScriptRequest::Definition { .. } => SemanticCapability::SymbolDefinition,
        TypeScriptRequest::TypeDefinition { .. } => SemanticCapability::TypeResolution,
        TypeScriptRequest::Implementation { .. } => SemanticCapability::ImplementationTarget,
        TypeScriptRequest::References { .. } => SemanticCapability::References,
        TypeScriptRequest::Hover { .. } | TypeScriptRequest::SignatureHelp { .. } => {
            SemanticCapability::OverloadResolution
        }
        TypeScriptRequest::DocumentSymbol { .. } => SemanticCapability::SyntaxStructure,
        TypeScriptRequest::PrepareCallHierarchy { .. }
        | TypeScriptRequest::IncomingCalls { .. }
        | TypeScriptRequest::OutgoingCalls { .. } => SemanticCapability::CallsCrossFile,
        TypeScriptRequest::WatchedFilesChanged { .. } => SemanticCapability::ResourceDiscovery,
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
    /// method it does not implement. Never an empty success: an
    /// unsupported capability is not zero findings.
    Protocol(String),
}

impl fmt::Display for AdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(failure) => write!(formatter, "{failure}"),
            Self::Protocol(detail) => write!(formatter, "unexpected backend answer: {detail}"),
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

/// What a backend location became.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Endpoint(GraphEndpoint),
    /// The location is inside the Workspace but names no Symbol. A
    /// re-export line is the ordinary case, and it is worth chasing
    /// rather than dropping.
    WorkspaceWithoutSymbol(Location),
    /// The location is real but cannot be named in Brainprint terms
    /// without guessing. Kept as a reason, never dropped.
    Unmappable(UnresolvedReason),
}

/// Resolves backend locations against the current Workspace.
pub struct Normalizer<'a> {
    connection: &'a Connection,
    workspace_root: &'a Path,
    /// The encoding the handshake settled on. Never assumed: reading a
    /// UTF-8 range as UTF-16 puts every span at a plausible wrong
    /// offset, which is worse than no span at all.
    encoding: PositionEncodingChoice,
    /// Source text of Resources already read, so one pass reads each
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
        encoding: PositionEncodingChoice,
    ) -> Self {
        Self {
            connection,
            workspace_root,
            encoding,
            texts: BTreeMap::new(),
            read: BTreeMap::new(),
        }
    }

    /// Every Resource read while resolving, with the revision it was
    /// read at. These belong in the task 3 basis: the answer depended
    /// on them, so it goes stale when they move.
    #[must_use]
    pub const fn sources_read(&self) -> &BTreeMap<ResourceId, String> {
        &self.read
    }

    #[must_use]
    pub const fn encoding(&self) -> PositionEncodingChoice {
        self.encoding
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
            // Physically inside the Workspace tree, semantically a
            // dependency. It is not indexed and must not become a
            // Resource here.
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
    ) -> Result<Target, AdapterError> {
        let Some(resource) = self.resource_for_uri(&location.uri)? else {
            return Ok(self.external_target(&location.uri, written_name));
        };
        // A whole-file range is the backend naming a module, not a
        // declaration inside one.
        if location.range.is_empty() {
            return Ok(Target::Endpoint(GraphEndpoint::Resource(resource.id)));
        }
        let encoding = self.encoding;
        let Some(text) = self.text_of(&resource) else {
            return Ok(Target::Unmappable(UnresolvedReason::NoStructuralBinding));
        };
        let Ok((start, end)) = encoding.line_map(text).span(location.range) else {
            // A range that does not convert exactly is refused rather
            // than clamped: the nearest span is a different symbol.
            return Ok(Target::Unmappable(UnresolvedReason::NoStructuralBinding));
        };
        match symbol_at_span(self.connection, resource.id, start, end).map_err(sqlite)? {
            SymbolMatch::One(symbol) => Ok(Target::Endpoint(GraphEndpoint::Symbol(symbol))),
            // Two Brainprint Symbols could be meant and nothing in the
            // answer chooses. Ordering is not evidence.
            SymbolMatch::Ambiguous => Ok(Target::Unmappable(UnresolvedReason::AmbiguousCandidates)),
            SymbolMatch::None => Ok(Target::WorkspaceWithoutSymbol(location.clone())),
        }
    }

    fn external_target(&self, uri: &str, written_name: Option<&str>) -> Target {
        let Some(path) = uri_to_path(uri) else {
            return Target::Unmappable(UnresolvedReason::NoStructuralBinding);
        };
        match external_entity(&path, written_name) {
            Some(entity) => Target::Endpoint(GraphEndpoint::External(entity)),
            // Outside the Workspace and outside any package: there is
            // no stable identity to give it.
            None => Target::Unmappable(UnresolvedReason::NoStructuralBinding),
        }
    }
}

/// Give a dependency declaration a stable package identity.
///
/// Derived from the package layout alone -- the path after the last
/// `node_modules` segment -- because that is what npm, pnpm and yarn
/// all agree on however differently they store the tree. No dependency
/// source is read, no `package.json` inside the dependency is opened,
/// and no Resource row is created: a package, a module path inside it,
/// and the name the referring site wrote. That is enough to point at
/// the same thing every time without mirroring `node_modules` into the
/// index.
///
/// A declaration file collapses onto the module it declares:
/// `path.d.ts` and `path.js` are both the module `path`, and
/// `index.d.ts` is the package itself.
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
        // `@scope/name` is one package name.
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
        .filter(|name| !name.is_empty() && name.chars().all(is_name_character))
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
        // The lifecycle owns dependency versions, and a locator would
        // be a machine path in canonical identity.
        resolved_version: None,
        declaration_locator: None,
    })
}

fn is_name_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '$'
}

/// `path.d.ts` -> `path`, `service.js` -> `service`, `types` -> `types`.
fn strip_module_extension(file: &str) -> &str {
    for suffix in [
        ".d.ts", ".d.mts", ".d.cts", ".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs",
    ] {
        if let Some(stem) = file.strip_suffix(suffix) {
            return stem;
        }
    }
    file
}

// ---------------------------------------------------------------------
// The sites one Resource offers
// ---------------------------------------------------------------------

/// The relation kinds this adapter resolves from a source occurrence.
///
/// Each one is a site I3 already recorded, so the answer has somewhere
/// exact to anchor. Unlike Python, `Implements` is here: TypeScript
/// writes an `implements` clause and I3 records it, so the relation is
/// a binding at a real site rather than something to derive.
/// `OVERRIDES` is still absent, because it is not written at a site at
/// all -- it is derived from proven inheritance
/// (see [`crate::semantic_overrides`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvableKind {
    Imports,
    Calls,
    References,
    UsesType,
    Extends,
    Implements,
}

impl ResolvableKind {
    const fn of_relation(kind: RelationKind) -> Option<Self> {
        match kind {
            RelationKind::Imports => Some(Self::Imports),
            RelationKind::Calls => Some(Self::Calls),
            RelationKind::References => Some(Self::References),
            RelationKind::UsesType => Some(Self::UsesType),
            RelationKind::Extends => Some(Self::Extends),
            RelationKind::Implements => Some(Self::Implements),
            RelationKind::Overrides | RelationKind::UsesEnv | RelationKind::UsesConfig => None,
        }
    }

    const fn of_intended(intended: IntendedRelation) -> Option<Self> {
        match intended {
            IntendedRelation::Known(kind) => Self::of_relation(kind),
            // TypeScript states the distinction in syntax, so a base
            // entry whose kind I3 could not settle is still an
            // `extends` clause -- an `implements` clause is recorded as
            // `IMPLEMENTS` or not at all.
            IntendedRelation::Inheritance => Some(Self::Extends),
        }
    }

    /// The default for an occurrence kind that carries no stated
    /// relation. A type site is absent on purpose: `extends`,
    /// `implements` and an annotation are three different relations and
    /// the occurrence kind does not say which, so nothing is guessed.
    const fn of_occurrence(kind: OccurrenceKind) -> Option<Self> {
        match kind {
            OccurrenceKind::ImportSite => Some(Self::Imports),
            OccurrenceKind::CallSite => Some(Self::Calls),
            OccurrenceKind::ReferenceSite => Some(Self::References),
            OccurrenceKind::TypeSite | OccurrenceKind::Definition | OccurrenceKind::KeySite => None,
        }
    }

    /// The canonical relation this site becomes once it resolves.
    const fn relation_kind(self) -> RelationKind {
        match self {
            Self::Imports => RelationKind::Imports,
            Self::Calls => RelationKind::Calls,
            Self::References => RelationKind::References,
            Self::UsesType => RelationKind::UsesType,
            Self::Extends => RelationKind::Extends,
            Self::Implements => RelationKind::Implements,
        }
    }

    /// Which capability answered, which depends on where the target
    /// turned out to be.
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
            // A bound name rather than a module path: the thing the
            // backend had to follow through aliases and re-exports.
            Self::Imports => SemanticCapability::AliasResolution,
            Self::Calls if site.module_hint.is_some() => SemanticCapability::CallsCrossFile,
            Self::Calls => SemanticCapability::CallsIntraFile,
            Self::References => SemanticCapability::References,
            Self::UsesType => SemanticCapability::TypeResolution,
            Self::Extends => SemanticCapability::Inheritance,
            Self::Implements => SemanticCapability::Implements,
        }
    }

    /// A member call binds through the receiver's declared type, and a
    /// subclass may still take over at run time. Saying `STATIC` there
    /// would claim more than the backend proved.
    fn dispatch(self, site: &SemanticSite) -> Dispatch {
        match self {
            Self::Calls
                if site.module_hint.is_some()
                    || site.reason == UnresolvedReason::ReceiverTypeRequired =>
            {
                Dispatch::Unknown
            }
            _ => Dispatch::Static,
        }
    }
}

/// One site the adapter will ask about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticSite {
    pub occurrence: OccurrenceRef,
    pub kind: ResolvableKind,
    /// The name the source writes, used only to name an external
    /// target.
    pub lookup_name: String,
    /// The receiver a member call was written through, when there is
    /// one.
    pub module_hint: Option<String>,
    /// What the structural tier said when it could not settle the site.
    pub reason: UnresolvedReason,
    /// Whether the written text is a quoted module specifier rather
    /// than a name.
    pub specifier: bool,
}

/// A site the adapter deliberately left to the structural tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedExternal {
    pub occurrence: OccurrenceRef,
    pub kind: RelationKind,
    /// Why. Constant today, and a field rather than a comment so a
    /// report reads as evidence rather than as a bare list of spans.
    pub reason: &'static str,
}

/// Why a site the structural tier bound to a package is left alone.
pub const STRUCTURALLY_EXTERNAL: &str =
    "the structural tier bound this site to a dependency package";

/// A gap this tier does not answer, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredGap {
    pub occurrence: OccurrenceRef,
    pub intended: IntendedRelation,
    pub reason: &'static str,
}

/// Why an unanchored relation kind is left alone.
const NO_STATED_RELATION: &str = "no gap and no canonical relation states what this site means";

/// What the adapter will ask about for one Resource, and what it will
/// not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SiteSet {
    pub sites: Vec<SemanticSite>,
    pub retained_external: Vec<RetainedExternal>,
    pub deferred: Vec<DeferredGap>,
}

/// Collect every site in `owner` this adapter may answer.
///
/// Driven by the Occurrence table rather than by the gap table alone,
/// and that difference is the whole TypeScript story: `import { Service }
/// from "@core/service"` leaves *no* gap for `Service` -- I3 recorded
/// the import site and moved on -- so a gap-only pass would never prove
/// what the name binds to. The set is still bounded by what I3
/// recorded, which is what keeps this enrichment rather than a second
/// index of the program.
///
/// # Errors
/// When the index cannot be read.
pub fn collect_sites(
    connection: &Connection,
    owner: &Resource,
    owner_text: &str,
    gaps: &[PersistedUnresolved],
    context_key: &str,
) -> Result<SiteSet, rusqlite::Error> {
    let mut statement = connection.prepare(
        "SELECT occurrence.kind, occurrence.start_byte, occurrence.end_byte, relation.kind, \
                CASE WHEN target.external_entity_id IS NOT NULL THEN 1 ELSE 0 END, \
                CASE WHEN semantic_evidence.occurrence_id IS NOT NULL THEN 1 ELSE 0 END \
         FROM occurrence \
         JOIN resource ON resource.id = occurrence.resource_id \
         LEFT JOIN relation ON relation.id = occurrence.relation_id \
         LEFT JOIN graph_entity AS target ON target.id = relation.target_entity_id \
         LEFT JOIN semantic_evidence \
                ON semantic_evidence.occurrence_id = occurrence.id \
               AND semantic_evidence.context_key = ?2 \
         WHERE resource.uid = ?1 \
         ORDER BY occurrence.start_byte, occurrence.end_byte, occurrence.kind",
    )?;
    type Row = (String, i64, i64, Option<String>, i64, i64);
    let rows: Vec<Row> = statement
        .query_map(params![owner.id.to_bytes().to_vec(), context_key], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })?
        .collect::<Result<_, _>>()?;

    let mut found = SiteSet::default();
    for (raw_kind, start, end, bound_kind, external, ours) in rows {
        let Ok(occurrence_kind) = OccurrenceKind::parse_public(&raw_kind) else {
            continue;
        };
        let site = OccurrenceRef {
            kind: occurrence_kind,
            start_byte: usize::try_from(start).unwrap_or(0),
            end_byte: usize::try_from(end).unwrap_or(0),
        };
        if ResolvableKind::of_occurrence(occurrence_kind).is_none()
            && occurrence_kind != OccurrenceKind::TypeSite
        {
            continue;
        }

        let gap = gaps.iter().find(|gap| gap.occurrence == site);
        let bound = bound_kind.as_deref().and_then(|kind| {
            RelationKind::parse(kind)
                .ok()
                .and_then(ResolvableKind::of_relation)
        });

        // A binding this context established last time is this
        // context's own previous answer, not the structural tier's
        // claim: re-asking it is what makes a repeated refresh
        // idempotent (task 4 `own_binding` agrees).
        if external == 1 && ours == 0 {
            if let Some(kind) = bound {
                found.retained_external.push(RetainedExternal {
                    occurrence: site,
                    kind: kind.relation_kind(),
                    reason: STRUCTURALLY_EXTERNAL,
                });
            }
            continue;
        }

        let Some(kind) = gap
            .and_then(|gap| ResolvableKind::of_intended(gap.intended))
            .or(bound)
            .or_else(|| ResolvableKind::of_occurrence(occurrence_kind))
        else {
            if let Some(gap) = gap {
                found.deferred.push(DeferredGap {
                    occurrence: site,
                    intended: gap.intended,
                    reason: NO_STATED_RELATION,
                });
            }
            continue;
        };

        let written = owner_text
            .get(site.start_byte..site.end_byte)
            .unwrap_or_default();
        found.sites.push(SemanticSite {
            occurrence: site,
            kind,
            lookup_name: gap.map_or_else(|| written.to_owned(), |gap| gap.lookup_name.clone()),
            module_hint: gap.and_then(|gap| gap.module_hint.clone()),
            reason: gap.map_or(UnresolvedReason::NoStructuralBinding, |gap| gap.reason),
            specifier: is_specifier(written),
        });
    }
    Ok(found)
}

/// Whether the written text is a quoted module specifier.
fn is_specifier(written: &str) -> bool {
    let mut characters = written.chars();
    matches!(characters.next(), Some('"' | '\'' | '`'))
}

// ---------------------------------------------------------------------
// Resolving one Resource
// ---------------------------------------------------------------------

/// What one Resource's semantic pass produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceEvidence {
    pub evidence: Vec<SemanticEvidence>,
    /// Sites left with the structural tier's dependency classification.
    pub retained_external: Vec<RetainedExternal>,
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
    pub sites: &'a SiteSet,
    pub occurrences: &'a [Occurrence],
    pub generation_id: i64,
    pub analysis_profile_id: i64,
    pub context_key: &'a str,
}

/// Ask the backend about every site this Resource offers, and normalize
/// the answers.
///
/// # Errors
/// When the backend cannot be reached or answers a shape this build
/// does not read.
pub fn resolve_resource(
    queries: &dyn TypeScriptQueries,
    normalizer: &mut Normalizer<'_>,
    request: &ResourceRequest<'_>,
) -> Result<ResourceEvidence, AdapterError> {
    let owner_uri = path_to_uri(&normalizer.workspace_root.join(&request.owner.path_rel));
    let map = normalizer.encoding.line_map(request.owner_text);
    let mut produced = ResourceEvidence {
        retained_external: request.sites.retained_external.clone(),
        deferred: request.sites.deferred.clone(),
        ..ResourceEvidence::default()
    };

    for site in &request.sites.sites {
        let outcome = resolve_one(queries, normalizer, &map, &owner_uri, site)?;
        let source = source_endpoint(request.owner.id, request.occurrences, site.occurrence);

        if site.kind == ResolvableKind::Extends
            && let SemanticOutcome::Resolved {
                target: GraphEndpoint::Symbol(base),
            } = &outcome
            && let GraphEndpoint::Symbol(subclass) = source
        {
            // Remembered so override derivation in this same pass can
            // use a base this pass has only just established.
            produced.resolved_bases.push((subclass, *base));
        }

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
            dispatch: site.kind.dispatch(site),
        });
    }
    Ok(produced)
}

/// Ask about one site and chase a re-export answer to its declaration.
///
/// The chase is what makes `export { Model as PublicModel } from
/// "./model.js"` work. I3 records no Occurrence for a re-export line,
/// so a definition that lands on one names a Workspace file and no
/// Symbol; asking again *at that place* is how the server is made to
/// take the next hop. It is still the server deciding every hop --
/// nothing here reads the export syntax.
fn resolve_one(
    queries: &dyn TypeScriptQueries,
    normalizer: &mut Normalizer<'_>,
    map: &LineMap<'_>,
    owner_uri: &str,
    site: &SemanticSite,
) -> Result<SemanticOutcome, AdapterError> {
    // A module specifier is asked about *inside* the quotes; a name is
    // asked about at its last character, because I3 records a call site
    // at the whole callee and asking at the start of `x.run` answers
    // about the receiver `x` -- a real, wrong target.
    let position = if site.specifier {
        map.position(site.occurrence.start_byte + 1)
    } else {
        map.last_character_position(site.occurrence.start_byte, site.occurrence.end_byte)
    };
    let Ok(position) = position else {
        // A span that does not convert exactly is never asked about:
        // the nearest position is a different token, and the backend
        // would answer about that one.
        return Ok(SemanticOutcome::Unresolved {
            reason: UnresolvedReason::NoStructuralBinding,
        });
    };

    let name =
        (!site.lookup_name.is_empty() && !site.specifier).then_some(site.lookup_name.clone());
    let mut uri = owner_uri.to_owned();
    let mut at = position;
    let mut targets: Vec<Target> = Vec::new();
    for _ in 0..MAX_REEXPORT_HOPS {
        targets = definition(queries, normalizer, &uri, at, name.as_deref())?;
        // Exactly one answer, and it landed on a Workspace file that
        // declares no Symbol: a re-export. Take the next hop.
        let [Target::WorkspaceWithoutSymbol(location)] = targets.as_slice() else {
            break;
        };
        let next = location.clone();
        if next.uri == uri && next.range.start == at {
            // The server answered "here", which is where the question
            // was asked. Chasing further would loop.
            break;
        }
        uri = next.uri;
        at = next.range.start;
    }

    Ok(outcome_of(targets, site))
}

fn definition(
    queries: &dyn TypeScriptQueries,
    normalizer: &mut Normalizer<'_>,
    uri: &str,
    position: crate::lsp::coordinates::Position,
    written_name: Option<&str>,
) -> Result<Vec<Target>, AdapterError> {
    let locations = match queries.call(&TypeScriptRequest::Definition {
        uri: uri.to_owned(),
        position,
    })? {
        TypeScriptResponse::Locations(locations) => locations,
        TypeScriptResponse::Unsupported(method) => {
            return Err(AdapterError::Protocol(format!("{method} is unsupported")));
        }
        other => return Err(AdapterError::Protocol(format!("{other:?}"))),
    };
    let mut targets = Vec::new();
    for location in &locations {
        targets.push(normalizer.target_for(location, written_name)?);
    }
    Ok(targets)
}

fn outcome_of(targets: Vec<Target>, site: &SemanticSite) -> SemanticOutcome {
    let mut endpoints: Vec<GraphEndpoint> = Vec::new();
    let mut unmappable: Option<UnresolvedReason> = None;
    for target in targets {
        match target {
            Target::Endpoint(endpoint) => {
                // A declaration and its `.d.ts` normalize to the same
                // external module, so two answers become one target.
                if !endpoints.contains(&endpoint) {
                    endpoints.push(endpoint);
                }
            }
            Target::WorkspaceWithoutSymbol(_) => {
                unmappable = unmappable.or(Some(UnresolvedReason::NoStructuralBinding));
            }
            Target::Unmappable(reason) => unmappable = unmappable.or(Some(reason)),
        }
    }
    if let Some(merged) = merged_declaration(&endpoints) {
        endpoints = vec![GraphEndpoint::External(merged)];
    }
    match endpoints.len() {
        1 => SemanticOutcome::Resolved {
            target: endpoints.remove(0),
        },
        0 => SemanticOutcome::Unresolved {
            // Keeping I3's reason when the backend added nothing is
            // more honest than replacing it with a new one.
            reason: unmappable.unwrap_or(site.reason),
        },
        // Several distinct targets: the backend did not settle it, so
        // neither does this.
        _ => SemanticOutcome::Candidates { targets: endpoints },
    }
}

/// Collapse a declaration-merged external answer into one entity.
///
/// TypeScript's interface and namespace merging is not an ambiguity: a
/// name declared across several `.d.ts` files of one package *is* one
/// declaration, and the server answers with every file that
/// contributes. `String` in `typescript/lib` comes back nine times --
/// `lib.es5.d.ts`, `lib.es2015.core.d.ts` and the rest -- and reporting
/// that as nine candidate targets would turn the most ordinary global
/// in the language into an unresolved site.
///
/// Only collapsed when every answer agrees on the package *and* on the
/// name, which is exactly when merging is what happened. Which
/// declaration file contributed is then not identity, so the module
/// path is dropped rather than one of them being picked.
fn merged_declaration(endpoints: &[GraphEndpoint]) -> Option<ExternalEntity> {
    if endpoints.len() < 2 {
        return None;
    }
    let mut entities = endpoints.iter().map(|endpoint| match endpoint {
        GraphEndpoint::External(entity) => Some(entity),
        _ => None,
    });
    let first = entities.next()??;
    let symbol_name = first.symbol_name.clone()?;
    for entity in entities {
        let entity = entity?;
        if entity.package_identity != first.package_identity
            || entity.symbol_name.as_deref() != Some(symbol_name.as_str())
        {
            return None;
        }
    }
    Some(ExternalEntity {
        package_identity: first.package_identity.clone(),
        qualified_name: Some(symbol_name.clone()),
        kind: EXTERNAL_SYMBOL.to_owned(),
        module_path: None,
        symbol_name: Some(symbol_name),
        resolved_version: None,
        declaration_locator: None,
    })
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
// Override claims
// ---------------------------------------------------------------------

/// Which members of `owner` are written with the `override` keyword.
///
/// Syntax only, and validation evidence only: it never creates an edge.
/// A claim whose ancestor member cannot be proven is reported as an
/// honest gap by [`crate::semantic_overrides`], and a member without
/// the keyword still gets an `OVERRIDES` edge when the inheritance
/// proves one -- TypeScript only requires the keyword under
/// `noImplicitOverride`.
///
/// Read from the text between the declaration's own start and its name,
/// which is exactly where a modifier list can be. No name matching, and
/// nothing outside the declaration is consulted.
#[must_use]
pub fn override_keyword_members(
    owner_text: &str,
    symbols: &[Symbol],
    occurrences: &[Occurrence],
) -> BTreeSet<SymbolId> {
    let mut declared = BTreeSet::new();
    for member in symbols.iter().filter(|symbol| {
        matches!(
            symbol.kind,
            SymbolKind::Method | SymbolKind::Property | SymbolKind::Field
        )
    }) {
        let Some(name_site) = occurrences.iter().find(|occurrence| {
            occurrence.kind == OccurrenceKind::Definition
                && occurrence.containing_symbol_id == Some(member.id)
        }) else {
            continue;
        };
        let Some(modifiers) = owner_text.get(member.span.start_byte..name_site.span.start_byte)
        else {
            continue;
        };
        if modifiers
            .split(|character: char| !is_name_character(character))
            .any(|word| word == "override")
        {
            declared.insert(member.id);
        }
    }
    declared
}

// ---------------------------------------------------------------------
// Filesystem synchronization
// ---------------------------------------------------------------------

/// Tell the backend that files changed on disk.
///
/// The only synchronization this tier performs, and the barrier the
/// whole freshness model rests on: a notification travels on the same
/// ordered stdio connection as the requests that follow it, so "the
/// backend has caught up" is a fact about message order rather than
/// about elapsed time. See [`super::WATCHER_DECISION`].
///
/// # Errors
/// When the notification could not be delivered.
pub fn notify_watched_files(
    queries: &dyn TypeScriptQueries,
    changes: Vec<super::protocol::WatchedChange>,
) -> Result<(), RequestFailure> {
    queries
        .call(&TypeScriptRequest::WatchedFilesChanged { changes })
        .map(|_| ())
}

/// The absolute path a Workspace-relative one names, as a backend URI.
#[must_use]
pub fn uri_for(workspace_root: &Path, path_rel: &str) -> String {
    path_to_uri(&workspace_root.join(path_rel))
}

/// Exposed for the lifecycle, which needs the same `node_modules`
/// awareness when it decides whether a path is dependency territory.
#[must_use]
pub fn is_dependency_path(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == NODE_MODULES)
}

/// A dependency root under `workspace_root`, for tests and diagnostics.
#[must_use]
pub fn dependency_root(workspace_root: &Path) -> PathBuf {
    workspace_root.join(NODE_MODULES)
}
