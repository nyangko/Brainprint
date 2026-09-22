//! Turning rust-analyzer answers into normalized Brainprint evidence.
//!
//! ```text
//! LSP URI · UTF-16 range
//!         ↓  adapter
//! ResourceId · SymbolId · ExternalEntity · byte SourceSpan
//! ```
//!
//! Three things here are Rust-shaped and nothing else is.
//!
//! **A module answers as a file.** `use bp_core::runner` comes back as
//! the whole of `runner.rs`, from `0:0` past its last line. A module is
//! not a declaration and Rust writes it three ways — `runner.rs`,
//! `runner/mod.rs`, `mod runner { … }` — so the canonical target is the
//! [`Resource`], and nothing here invents a module Symbol to look like
//! rust-analyzer's own crate graph.
//!
//! **Dispatch is readable from the answer.** A call on a concrete type
//! resolves to the member inside an `impl`; the same call through
//! `&dyn Trait` resolves to the member inside the `trait`. So the two
//! are told apart by *where the compiler pointed*, not by parsing
//! hover text or by inventing a Rust dispatch vocabulary.
//!
//! **A dependency is an identity, never a Resource.** A target under a
//! registry checkout, a git checkout or the sysroot is normalized to an
//! [`ExternalEntity`] keyed on the crate and version Cargo proved, and
//! its source is never indexed, persisted or handed to an Agent.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
};

use brainprint_core::{ResourceId, SymbolId};
use rusqlite::Connection;

use super::protocol::{
    Location, POSITION_ENCODING, RustRequest, RustResponse, is_dependency_path, is_virtual_uri,
    path_to_uri, uri_to_path,
};
use crate::{
    gaps::UnresolvedReason,
    graph::{ExternalEntity, GraphEndpoint, RelationKind},
    lsp::coordinates::{LineMap, Range},
    resolution::{Dispatch, EvidenceBasis, Support},
    resource::Resource,
    runtime::{RequestFailure, RequestOptions, RuntimeLease, RuntimeRequest, SemanticRequestKey},
    semantic::{SemanticCapability, SemanticEvidence, SemanticOutcome},
    semantic_normalize::{SymbolMatch, resource_by_path_key, symbol_at_span},
    symbol::{Occurrence, OccurrenceKind, Symbol, SymbolKind},
};

/// The site vocabulary and Workspace lookups shared with every other
/// adapter.
pub use crate::semantic_normalize::{
    DeferredGap, ResolvableKind, RetainedExternal, SemanticSite, SiteSet, collect_sites,
    displaced_gaps, resource_by_id, source_endpoint,
};

/// The `ExternalEntity::kind` for a crate outside the Workspace.
pub const EXTERNAL_CRATE: &str = "RUST_CRATE";
/// The `ExternalEntity::kind` for an item inside one.
pub const EXTERNAL_ITEM: &str = "RUST_ITEM";

// ---------------------------------------------------------------------
// Asking the backend
// ---------------------------------------------------------------------

/// Whatever can answer a typed Rust question.
pub trait RustQueries {
    /// # Errors
    /// When the backend could not be reached or refused the request.
    fn call(&self, request: &RustRequest) -> Result<RustResponse, RequestFailure>;
}

/// Asks through the task 2 supervisor.
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

impl RustQueries for LeaseQueries<'_> {
    fn call(&self, request: &RustRequest) -> Result<RustResponse, RequestFailure> {
        let payload = serde_json::to_vec(request).expect("a RustRequest is always serializable");
        let key = SemanticRequestKey {
            context_key: self.lease.context_key().to_owned(),
            capability: match request {
                RustRequest::Implementation { .. } => SemanticCapability::ImplementationTarget,
                RustRequest::References { .. } => SemanticCapability::References,
                RustRequest::DocumentSymbol { .. } => SemanticCapability::SyntaxStructure,
                _ => SemanticCapability::SymbolDefinition,
            },
            target: String::from_utf8_lossy(&payload).into_owned(),
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
                "undecodable Rust response: {error}"
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
    Protocol(String),
    Index(String),
}

impl fmt::Display for AdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(failure) => write!(formatter, "{failure}"),
            Self::Protocol(detail) => write!(formatter, "unexpected backend answer: {detail}"),
            Self::Index(detail) => write!(formatter, "index: {detail}"),
        }
    }
}

impl Error for AdapterError {}

impl From<RequestFailure> for AdapterError {
    fn from(failure: RequestFailure) -> Self {
        Self::Request(failure)
    }
}

fn index(error: impl fmt::Display) -> AdapterError {
    AdapterError::Index(error.to_string())
}

// ---------------------------------------------------------------------
// External identity
// ---------------------------------------------------------------------

/// The crate a path outside the Workspace belongs to.
///
/// Derived from the shape Cargo and rustup lay down, and from nothing
/// machine-specific:
///
/// ```text
/// …/.cargo/registry/src/index.crates.io-6f17d22bba15001f/serde-1.0.219/src/lib.rs
///                                                        ^^^^^^^^^^^^^  name-version
/// …/.cargo/git/checkouts/thing-abc123/9f2c1/src/lib.rs
///                        ^^^^^^^^^^^^        name-revision
/// …/toolchains/stable-aarch64-apple-darwin/lib/rustlib/src/rust/library/core/src/option.rs
///                                                                      ^^^^  the std crate
/// ```
///
/// What never appears is the part before those: a home directory, a
/// registry hash directory, a toolchain path. Two machines with the same
/// dependency must produce the same identity, and an absolute path
/// cannot.
#[must_use]
pub fn crate_identity(path: &Path) -> Option<(String, Option<String>)> {
    let text = path.to_string_lossy().replace('\\', "/");
    if let Some(after) = text.split("/.cargo/registry/src/").nth(1) {
        // Skip the registry-hash directory, take `name-version`.
        let named = after.split('/').nth(1)?;
        let (name, version) = named.rsplit_once('-')?;
        return Some((name.to_owned(), Some(version.to_owned())));
    }
    if let Some(after) = text.split("/.cargo/git/checkouts/").nth(1) {
        let named = after.split('/').next()?;
        let (name, _hash) = named.rsplit_once('-')?;
        return Some((name.to_owned(), None));
    }
    if let Some(after) = text.split("/rustlib/src/rust/library/").nth(1) {
        return Some((after.split('/').next()?.to_owned(), None));
    }
    None
}

/// A dependency's item, as canonical identity.
///
/// The crate Cargo proved, the version it pinned, and the name the
/// referring site wrote. No absolute path, no registry hash, no
/// toolchain directory.
#[must_use]
pub fn external_entity(path: &Path, written_name: Option<&str>) -> Option<ExternalEntity> {
    let (package, version) = crate_identity(path)?;
    let item = written_name
        .and_then(|name| name.rsplit("::").next())
        .map(str::trim)
        .filter(|name| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|character| character.is_alphanumeric() || character == '_')
        })
        .map(str::to_owned);
    Some(ExternalEntity {
        package_identity: package.clone(),
        qualified_name: item.as_ref().map_or_else(
            || Some(package.clone()),
            |item| Some(format!("{package}::{item}")),
        ),
        kind: if item.is_some() {
            EXTERNAL_ITEM.to_owned()
        } else {
            EXTERNAL_CRATE.to_owned()
        },
        module_path: Some(package),
        symbol_name: item,
        resolved_version: version,
        // A checkout path is a locator, never identity.
        declaration_locator: None,
    })
}

// ---------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------

/// What a backend location became.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Endpoint(GraphEndpoint),
    /// Inside the Workspace, but naming no Symbol this build can point
    /// at.
    Unmappable(UnresolvedReason),
    /// Generated or virtual, and therefore refused.
    Refused(MappingFailure),
}

/// Why a location was refused rather than normalized.
///
/// The Rust analogue of the Svelte generated-source gate. A macro
/// expansion is not something a person can edit, so an answer that
/// names one cannot become a canonical span: an Agent handed that text
/// would be handed something it cannot change, described as something
/// it can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingFailure {
    /// A `rust-analyzer://` document, or a macro expansion.
    VirtualDocument,
    /// Real, on disk, and inside build output.
    GeneratedOutput,
}

impl fmt::Display for MappingFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::VirtualDocument => "a virtual or macro-expansion document",
            Self::GeneratedOutput => "generated build output",
        })
    }
}

/// Resolves backend locations against the current Workspace.
pub struct Normalizer<'a> {
    pub(crate) connection: &'a Connection,
    workspace_root: &'a Path,
    texts: BTreeMap<ResourceId, String>,
    read: BTreeMap<ResourceId, String>,
    refused: Vec<(MappingFailure, String)>,
}

impl<'a> Normalizer<'a> {
    #[must_use]
    pub fn new(connection: &'a Connection, workspace_root: &'a Path) -> Self {
        Self {
            connection,
            workspace_root,
            texts: BTreeMap::new(),
            read: BTreeMap::new(),
            refused: Vec::new(),
        }
    }

    /// Every Resource whose current bytes a resolution depended on.
    #[must_use]
    pub const fn sources_read(&self) -> &BTreeMap<ResourceId, String> {
        &self.read
    }

    /// Locations this pass refused, and why.
    #[must_use]
    pub fn refused(&self) -> &[(MappingFailure, String)] {
        &self.refused
    }

    fn resource_for(&self, path: &Path) -> Result<Option<Resource>, AdapterError> {
        let Ok(relative) = path.strip_prefix(self.workspace_root) else {
            return Ok(None);
        };
        let path_key = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        resource_by_path_key(self.connection, &path_key).map_err(index)
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
    fn target_for(
        &mut self,
        location: &Location,
        written_name: Option<&str>,
    ) -> Result<Target, AdapterError> {
        // The hard gate, before anything else looks at it.
        if is_virtual_uri(&location.uri) {
            self.refused
                .push((MappingFailure::VirtualDocument, location.uri.clone()));
            return Ok(Target::Refused(MappingFailure::VirtualDocument));
        }
        let Some(path) = uri_to_path(&location.uri) else {
            return Ok(Target::Unmappable(UnresolvedReason::NoStructuralBinding));
        };
        // A dependency or the sysroot: an identity, and its source is
        // never indexed.
        if is_dependency_path(&path) {
            return Ok(self.dependency_target(&path, written_name));
        }
        let Some(resource) = self.resource_for(&path)? else {
            return Ok(Target::Unmappable(UnresolvedReason::NoStructuralBinding));
        };
        // A module: the whole file, which is the Resource.
        if location.is_whole_file() {
            self.read
                .insert(resource.id, resource.resource_revision.clone());
            return Ok(Target::Endpoint(GraphEndpoint::Resource(resource.id)));
        }
        let Some(text) = self.text_of(&resource) else {
            return Ok(Target::Unmappable(UnresolvedReason::NoStructuralBinding));
        };
        let Ok((start, end)) = LineMap::with_encoding(text, POSITION_ENCODING).span(location.range)
        else {
            return Ok(Target::Unmappable(UnresolvedReason::NoStructuralBinding));
        };
        match symbol_at_span(self.connection, resource.id, start, end).map_err(index)? {
            SymbolMatch::One(symbol) => Ok(Target::Endpoint(GraphEndpoint::Symbol(symbol))),
            SymbolMatch::Ambiguous | SymbolMatch::None => {
                Ok(Target::Unmappable(UnresolvedReason::NoStructuralBinding))
            }
        }
    }

    fn dependency_target(&mut self, path: &Path, written_name: Option<&str>) -> Target {
        // `target/` output is real source on disk and still not project
        // source: a build script wrote it, and build scripts do not run
        // under the P0 configuration anyway.
        let text = path.to_string_lossy();
        if text.contains("/target/") {
            self.refused
                .push((MappingFailure::GeneratedOutput, path.display().to_string()));
            return Target::Refused(MappingFailure::GeneratedOutput);
        }
        external_entity(path, written_name).map_or(
            Target::Unmappable(UnresolvedReason::NoStructuralBinding),
            |entity| Target::Endpoint(GraphEndpoint::External(entity)),
        )
    }
}

// ---------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------

/// Whether a resolved callee is reached through a trait.
///
/// Measured, and the reason no Rust-specific vocabulary is needed: a
/// call on a concrete type resolved to the member inside
/// `impl Runner for Worker`, and the same call through `&dyn Runner`
/// resolved to the member inside `trait Runner`. So the question "is
/// this statically bound" is answered by whose member the compiler
/// pointed at.
///
/// `Err` reads as unknown, because failing to read a declaration is not
/// evidence that it is concrete.
fn through_trait(connection: &Connection, target: SymbolId) -> Result<bool, rusqlite::Error> {
    use rusqlite::{OptionalExtension, params};
    let parent_kind: Option<Option<String>> = connection
        .query_row(
            "SELECT parent.kind FROM symbol \
             LEFT JOIN symbol AS parent ON parent.id = symbol.parent_symbol_id \
             WHERE symbol.uid = ?1",
            params![target.to_bytes().to_vec()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(matches!(
        parent_kind.flatten().as_deref(),
        Some("TRAIT") | Some("INTERFACE")
    ))
}

// ---------------------------------------------------------------------
// Resolving one Resource
// ---------------------------------------------------------------------

/// What one Resource's semantic pass produced.
#[derive(Debug, Clone, Default)]
pub struct ResourceEvidence {
    pub evidence: Vec<SemanticEvidence>,
    pub retained_external: Vec<RetainedExternal>,
    pub deferred: Vec<DeferredGap>,
    /// Locations refused as generated or virtual. Incomplete coverage,
    /// never a fabricated span.
    pub refused: Vec<(MappingFailure, String)>,
    /// Sites the backend withdrew rather than answered.
    pub withdrawn: Vec<crate::evidence::OccurrenceRef>,
    /// `(type, trait)` pairs this pass proved, so the trait-member
    /// derivation can use an edge that has not merged yet.
    pub implemented: Vec<(SymbolId, SymbolId)>,
}

/// One Resource's inputs for a semantic pass.
pub struct ResourceRequest<'a> {
    pub owner: &'a Resource,
    pub owner_text: &'a str,
    /// Which type each `impl Trait for Type` block implements, by the
    /// span of the trait name.
    ///
    /// The source of an `IMPLEMENTS` edge is the implementing *type*,
    /// not the file and not the impl block — and the trait reference
    /// sits outside every declaration's span, so the ordinary
    /// enclosing-symbol rule cannot find it. This is I3's own
    /// derivation, reused rather than repeated.
    pub implementors: &'a BTreeMap<(usize, usize), SymbolId>,
    pub sites: &'a SiteSet,
    pub occurrences: &'a [Occurrence],
    pub generation_id: i64,
    pub analysis_profile_id: i64,
    pub context_key: &'a str,
}

/// How this backend reads a shared [`ResolvableKind`].
trait RustSite {
    fn capability(self, outcome: &SemanticOutcome) -> SemanticCapability;
}

impl RustSite for ResolvableKind {
    fn capability(self, outcome: &SemanticOutcome) -> SemanticCapability {
        let external = matches!(
            outcome,
            SemanticOutcome::Resolved {
                target: GraphEndpoint::External(_)
            }
        );
        match self {
            _ if external => SemanticCapability::ExternalSymbolResolution,
            Self::Imports => SemanticCapability::ImportBinding,
            Self::Calls => SemanticCapability::CallsCrossFile,
            Self::References => SemanticCapability::References,
            Self::UsesType => SemanticCapability::TypeResolution,
            // Rust has no class inheritance. A base list this tier can
            // see is a trait, so the capability is Implements and the
            // relation kind is corrected below.
            Self::Extends | Self::Implements => SemanticCapability::Implements,
        }
    }
}

/// Ask once, and ask again if the backend withdrew the request.
///
/// `Ok(None)` means it withdrew both times. One retry rather than a
/// loop: a server that cancels while it re-analyses settles, and a
/// caller that retried forever would hang instead of reporting
/// incomplete coverage.
fn ask(
    queries: &dyn RustQueries,
    request: &RustRequest,
) -> Result<Option<Vec<Location>>, AdapterError> {
    for _ in 0..2 {
        match queries.call(request)? {
            RustResponse::Locations(locations) => return Ok(Some(locations)),
            RustResponse::Cancelled(_) => {}
            RustResponse::Unsupported(method) => {
                return Err(AdapterError::Protocol(format!("{method} is unsupported")));
            }
            other => return Err(AdapterError::Protocol(format!("{other:?}"))),
        }
    }
    Ok(None)
}

/// Ask the backend about every site this Resource offers, and normalize
/// the answers.
///
/// # Errors
/// When the backend cannot be reached or answers a shape this build
/// does not read.
pub fn resolve_resource(
    queries: &dyn RustQueries,
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
        let Ok(position) =
            map.last_character_position(site.occurrence.start_byte, site.occurrence.end_byte)
        else {
            continue;
        };
        let question = RustRequest::Definition {
            uri: owner_uri.clone(),
            position,
        };
        let Some(locations) = ask(queries, &question)? else {
            produced.withdrawn.push(site.occurrence);
            continue;
        };

        let name = (!site.lookup_name.is_empty()).then_some(site.lookup_name.clone());
        let outcome = normalize(normalizer, &locations, name.as_deref(), site)?;
        let source = request
            .implementors
            .get(&(site.occurrence.start_byte, site.occurrence.end_byte))
            .copied()
            .map_or_else(
                || source_endpoint(request.owner.id, request.occurrences, site.occurrence),
                GraphEndpoint::Symbol,
            );
        let relation_kind = relation_of(site.kind, &outcome, normalizer.connection);
        let dispatch = dispatch_of(site.kind, &outcome, normalizer.connection);

        if matches!(
            site.kind,
            ResolvableKind::Extends | ResolvableKind::Implements
        ) && let SemanticOutcome::Resolved {
            target: GraphEndpoint::Symbol(contract),
        } = &outcome
            && let GraphEndpoint::Symbol(implementor) = source
        {
            produced.implemented.push((implementor, *contract));
        }

        produced.evidence.push(SemanticEvidence {
            context_key: request.context_key.to_owned(),
            capability: site.kind.capability(&outcome),
            relation_kind: Some(relation_kind),
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
            dispatch,
        });
    }

    produced.refused = normalizer.refused().to_vec();
    Ok(produced)
}

/// Which relation a site produces, once the target's kind is known.
///
/// Rust has no class inheritance, so `EXTENDS` is never right here. A
/// base-list site whose target is a trait is `IMPLEMENTS`; anything
/// else stays what the structural tier intended. Supertraits
/// (`trait Detailed: Runner`) would need a relation that says
/// "requires", and overloading `EXTENDS` to mean it would state
/// something Rust does not have — so that capability is left PARTIAL
/// rather than misfiled.
fn relation_of(
    kind: ResolvableKind,
    outcome: &SemanticOutcome,
    connection: &Connection,
) -> RelationKind {
    if !matches!(kind, ResolvableKind::Extends | ResolvableKind::Implements) {
        return kind.relation_kind();
    }
    let SemanticOutcome::Resolved {
        target: GraphEndpoint::Symbol(target),
    } = outcome
    else {
        return kind.relation_kind();
    };
    match symbol_kind(connection, *target) {
        Some(SymbolKind::Trait | SymbolKind::Interface) => RelationKind::Implements,
        _ => kind.relation_kind(),
    }
}

/// How a call binds, once the target is known.
fn dispatch_of(
    kind: ResolvableKind,
    outcome: &SemanticOutcome,
    connection: &Connection,
) -> Dispatch {
    if kind != ResolvableKind::Calls {
        return Dispatch::Static;
    }
    let SemanticOutcome::Resolved {
        target: GraphEndpoint::Symbol(target),
    } = outcome
    else {
        return Dispatch::Static;
    };
    // A trait member is what a `dyn` call and a generic-bound call
    // resolve to; an inherent or concrete impl member is what a direct
    // call resolves to.
    if through_trait(connection, *target).unwrap_or(true) {
        Dispatch::Dynamic
    } else {
        Dispatch::Static
    }
}

fn symbol_kind(connection: &Connection, symbol: SymbolId) -> Option<SymbolKind> {
    use rusqlite::{OptionalExtension, params};
    let kind: Option<String> = connection
        .query_row(
            "SELECT kind FROM symbol WHERE uid = ?1",
            params![symbol.to_bytes().to_vec()],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten();
    kind.and_then(|kind| SymbolKind::parse_public(&kind).ok())
}

/// One answer set becomes one outcome.
fn normalize(
    normalizer: &mut Normalizer<'_>,
    locations: &[Location],
    written_name: Option<&str>,
    site: &SemanticSite,
) -> Result<SemanticOutcome, AdapterError> {
    let mut endpoints: Vec<GraphEndpoint> = Vec::new();
    let mut unmappable: Option<UnresolvedReason> = None;

    for location in locations {
        match normalizer.target_for(location, written_name)? {
            Target::Endpoint(endpoint) => {
                if !endpoints.contains(&endpoint) {
                    endpoints.push(endpoint);
                }
            }
            Target::Unmappable(reason) => unmappable = unmappable.or(Some(reason)),
            // Refused rather than published. Coverage, never a span.
            Target::Refused(_) => {
                unmappable = unmappable.or(Some(UnresolvedReason::NoStructuralBinding));
            }
        }
    }

    Ok(match endpoints.len() {
        1 => SemanticOutcome::Resolved {
            target: endpoints.remove(0),
        },
        0 => SemanticOutcome::Unresolved {
            reason: unmappable.unwrap_or(site.reason),
        },
        _ => SemanticOutcome::Candidates { targets: endpoints },
    })
}

// ---------------------------------------------------------------------
// Trait members
// ---------------------------------------------------------------------

/// Prove which trait member each implementing member satisfies.
///
/// Not derived from equal names, and the fixture is built so that a
/// name-based derivation would be caught: `Worker` implements both
/// `Runner` and `Reporter`, and both declare `run`, so two Symbols in
/// one file share the qualified name `Worker::run`.
///
/// The evidence is the compiler's. For each trait this owner was proved
/// to implement, the backend is asked `textDocument/implementation` at
/// each of that trait's members, and the answers landing in this owner
/// are that member's implementations. Asking from the trait's side and
/// anchoring on the *implementing* declaration keeps the edge owned by
/// the Resource that can replace it.
///
/// # Errors
/// When the backend cannot be reached or the index cannot be read.
pub fn derive_trait_members(
    queries: &dyn RustQueries,
    normalizer: &mut Normalizer<'_>,
    request: &ResourceRequest<'_>,
    implemented: &[(SymbolId, SymbolId)],
) -> Result<Vec<SemanticEvidence>, AdapterError> {
    let mut produced = Vec::new();
    let mut asked: BTreeSet<SymbolId> = BTreeSet::new();

    for (_implementor, contract) in implemented {
        for member in members_of(normalizer.connection, *contract).map_err(index)? {
            if !asked.insert(member.id) {
                continue;
            }
            let Some((trait_resource, trait_uri)) = locate(normalizer, member.resource_id)? else {
                continue;
            };
            let Some(trait_text) = fs::read_to_string(&trait_resource).ok() else {
                continue;
            };
            let Some(name_site) =
                member_name_span(normalizer.connection, member.id).map_err(index)?
            else {
                continue;
            };
            let Ok(position) = LineMap::with_encoding(&trait_text, POSITION_ENCODING)
                .last_character_position(name_site.0, name_site.1)
            else {
                continue;
            };
            let Some(locations) = ask(
                queries,
                &RustRequest::Implementation {
                    uri: trait_uri,
                    position,
                },
            )?
            else {
                continue;
            };

            for location in &locations {
                let Target::Endpoint(GraphEndpoint::Symbol(implementation)) =
                    normalizer.target_for(location, Some(&member.name))?
                else {
                    continue;
                };
                // Only this owner's declarations: another Resource's
                // implementation is that Resource's evidence, produced
                // when it is refreshed.
                let Some(site) = definition_site(request.occurrences, implementation) else {
                    continue;
                };
                produced.push(SemanticEvidence {
                    context_key: request.context_key.to_owned(),
                    capability: SemanticCapability::Implements,
                    relation_kind: Some(RelationKind::Implements),
                    basis: EvidenceBasis {
                        owner_resource: request.owner.id,
                        owner_resource_revision: request.owner.resource_revision.clone(),
                        generation_id: request.generation_id,
                        analysis_profile_id: request.analysis_profile_id,
                        resolution_context_key: None,
                    },
                    occurrence: Some(site),
                    source: Some(GraphEndpoint::Symbol(implementation)),
                    outcome: SemanticOutcome::Resolved {
                        target: GraphEndpoint::Symbol(member.id),
                    },
                    // Which member a declaration implements is written,
                    // not dispatched.
                    dispatch: Dispatch::Static,
                    support: Support::Supported,
                });
            }
        }
    }
    Ok(produced)
}

/// One member of a trait, with where it lives.
struct TraitMember {
    id: SymbolId,
    name: String,
    resource_id: ResourceId,
}

fn members_of(
    connection: &Connection,
    contract: SymbolId,
) -> Result<Vec<TraitMember>, rusqlite::Error> {
    let mut statement = connection.prepare(
        "SELECT member.uid, member.name, resource.uid FROM symbol AS member \
         JOIN symbol AS parent ON parent.id = member.parent_symbol_id \
         JOIN resource ON resource.id = member.resource_id \
         WHERE parent.uid = ?1 ORDER BY member.start_byte",
    )?;
    let rows: Vec<(Vec<u8>, String, Vec<u8>)> = statement
        .query_map(rusqlite::params![contract.to_bytes().to_vec()], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(uid, name, resource)| {
            Some(TraitMember {
                id: SymbolId::from_bytes(<[u8; 16]>::try_from(uid.as_slice()).ok()?),
                name,
                resource_id: ResourceId::from_bytes(
                    <[u8; 16]>::try_from(resource.as_slice()).ok()?,
                ),
            })
        })
        .collect())
}

/// The byte span of a member's own name, from its `DEFINITION`
/// Occurrence.
fn member_name_span(
    connection: &Connection,
    member: SymbolId,
) -> Result<Option<(usize, usize)>, rusqlite::Error> {
    use rusqlite::{OptionalExtension, params};
    let row: Option<(i64, i64)> = connection
        .query_row(
            "SELECT occurrence.start_byte, occurrence.end_byte FROM occurrence \
             JOIN symbol ON symbol.id = occurrence.containing_symbol_id \
             WHERE symbol.uid = ?1 AND occurrence.kind = 'DEFINITION' \
             ORDER BY occurrence.start_byte LIMIT 1",
            params![member.to_bytes().to_vec()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(row.map(|(start, end)| {
        (
            usize::try_from(start).unwrap_or_default(),
            usize::try_from(end).unwrap_or_default(),
        )
    }))
}

fn definition_site(
    occurrences: &[Occurrence],
    symbol: SymbolId,
) -> Option<crate::evidence::OccurrenceRef> {
    occurrences
        .iter()
        .find(|occurrence| {
            occurrence.kind == OccurrenceKind::Definition
                && occurrence.containing_symbol_id == Some(symbol)
        })
        .map(|occurrence| crate::evidence::OccurrenceRef {
            kind: occurrence.kind,
            start_byte: occurrence.span.start_byte,
            end_byte: occurrence.span.end_byte,
        })
}

fn locate(
    normalizer: &Normalizer<'_>,
    resource: ResourceId,
) -> Result<Option<(PathBuf, String)>, AdapterError> {
    let Some(found) = resource_by_id(normalizer.connection, resource).map_err(index)? else {
        return Ok(None);
    };
    let path = normalizer.workspace_root.join(&found.path_rel);
    let uri = path_to_uri(&path);
    Ok(Some((path, uri)))
}

// ---------------------------------------------------------------------
// Synchronization
// ---------------------------------------------------------------------

/// Hand the backend Brainprint's current bytes for one Resource.
///
/// # Errors
/// When the notification could not be delivered.
pub fn synchronize_document(
    queries: &dyn RustQueries,
    uri: String,
    text: String,
    version: i64,
    previous: Option<&str>,
) -> Result<(), RequestFailure> {
    let request = match previous {
        Some(previous) => RustRequest::ChangeDocument {
            uri,
            text,
            version,
            replaces: whole_of(previous),
        },
        None => RustRequest::OpenDocument { uri, text, version },
    };
    queries.call(&request).map(|_| ())
}

/// The range covering an entire document.
#[must_use]
pub fn whole_of(text: &str) -> Range {
    LineMap::with_encoding(text, POSITION_ENCODING)
        .range(0, text.len())
        .unwrap_or_else(|_| {
            Range::new(
                crate::lsp::coordinates::Position::new(0, 0),
                crate::lsp::coordinates::Position::new(0, 0),
            )
        })
}

/// Tell the backend the filesystem moved.
///
/// # Errors
/// When the notification could not be delivered.
pub fn notify_watched_files(
    queries: &dyn RustQueries,
    changes: Vec<super::protocol::WatchedChange>,
) -> Result<(), RequestFailure> {
    queries
        .call(&RustRequest::WatchedFilesChanged { changes })
        .map(|_| ())
}

/// Ask the backend to re-read the Cargo manifests.
///
/// Reading a manifest is reading what the Workspace wrote, so this is
/// trust-gated in the host.
///
/// # Errors
/// When the request could not be delivered or was refused.
pub fn reload_workspace(queries: &dyn RustQueries) -> Result<(), RequestFailure> {
    queries.call(&RustRequest::ReloadWorkspace).map(|_| ())
}

/// Whether a declaration is a trait member rather than an impl member.
///
/// Exposed for the capability report's own tests.
#[must_use]
pub fn declares_trait_member(connection: &Connection, symbol: SymbolId) -> bool {
    through_trait(connection, symbol).unwrap_or(false)
}

/// The symbols declared by `owner`, for callers that need them beside
/// the sites.
///
/// # Errors
/// When the index cannot be read.
pub fn own_symbols(
    connection: &Connection,
    owner: ResourceId,
) -> Result<Vec<Symbol>, AdapterError> {
    crate::symbol::list_for_resource(connection, owner).map_err(index)
}
