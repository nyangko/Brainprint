//! Turning Roslyn answers into normalized Brainprint evidence.
//!
//! ```text
//! LSP URI · UTF-16 range · decompiled metadata path
//!         ↓  adapter
//! ResourceId · SymbolId · LogicalSymbolId · ExternalEntity · byte SourceSpan
//! ```
//!
//! Two things here are C#-shaped and nothing else is.
//!
//! **A partial type answers with every declaration.** Asked for the
//! definition of `Runner`, the measured server returns both
//! `Runner.Part1.cs` and `Runner.Part2.cs`. That is not ambiguity and
//! not a candidate set -- the compiler is naming one type -- so it
//! becomes one binding to a
//! [`LogicalSymbol`](crate::logical_symbol), which owns those
//! declarations. A member declared once stays an ordinary
//! [`GraphEndpoint::Symbol`], because it is one.
//!
//! **A framework target is decompiled metadata.** `Console.WriteLine`
//! resolves into a temp file whose path is a content hash -- useless as
//! identity, and not source anyone wrote. Its *header* names the
//! assembly, and that is real identity; see [`assembly_identity`].
//!
//! ## What the trust gate changes here
//!
//! Nothing, deliberately. This module normalizes whatever the backend
//! answered; what the backend was *allowed to load* is decided before a
//! request is made ([`ProjectExecutionTrust`](super::protocol::ProjectExecutionTrust)),
//! and an untrusted Workspace simply gets fewer answers -- intra-document
//! ones, which were measured to execute no project code. An empty answer
//! is a gap either way, so there is no path here that turns a refusal
//! into a false fact.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs,
    path::Path,
};

use brainprint_core::{LogicalSymbolId, ResourceId, SymbolId};
use rusqlite::Connection;

use super::protocol::{
    CSharpRequest, CSharpResponse, Location, POSITION_ENCODING, is_metadata_uri, path_to_uri,
    uri_to_path,
};
use crate::{
    gaps::UnresolvedReason,
    graph::{ExternalEntity, GraphEndpoint},
    logical_symbol::{self, LogicalIdentity},
    lsp::coordinates::LineMap,
    resolution::{Dispatch, EvidenceBasis, Support},
    resource::Resource,
    runtime::{RequestFailure, RequestOptions, RuntimeLease, RuntimeRequest, SemanticRequestKey},
    semantic::{SemanticCapability, SemanticEvidence, SemanticOutcome},
    semantic_normalize::{SymbolMatch, resource_by_path_key, symbol_at_span},
    symbol::{Occurrence, Symbol, SymbolKind},
};

/// The site vocabulary and Workspace lookups shared with every other
/// adapter.
pub use crate::semantic_normalize::{
    DeferredGap, ResolvableKind, RetainedExternal, SemanticSite, SiteSet, collect_sites,
    displaced_gaps, resource_by_id, source_endpoint,
};

/// The `ExternalEntity::kind` for a referenced assembly.
pub const EXTERNAL_ASSEMBLY: &str = "DOTNET_ASSEMBLY";
/// The `ExternalEntity::kind` for a type or member inside one.
pub const EXTERNAL_SYMBOL: &str = "DOTNET_SYMBOL";

// ---------------------------------------------------------------------
// Asking the backend
// ---------------------------------------------------------------------

/// Whatever can answer a typed C# question.
pub trait CSharpQueries {
    /// # Errors
    /// When the backend could not be reached or refused the request.
    fn call(&self, request: &CSharpRequest) -> Result<CSharpResponse, RequestFailure>;
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

impl CSharpQueries for LeaseQueries<'_> {
    fn call(&self, request: &CSharpRequest) -> Result<CSharpResponse, RequestFailure> {
        let payload = serde_json::to_vec(request).expect("a CSharpRequest is always serializable");
        let key = SemanticRequestKey {
            context_key: self.lease.context_key().to_owned(),
            capability: match request {
                CSharpRequest::Implementation { .. } => SemanticCapability::ImplementationTarget,
                CSharpRequest::References { .. } => SemanticCapability::References,
                CSharpRequest::DocumentSymbol { .. } => SemanticCapability::SyntaxStructure,
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
                "undecodable C# response: {error}"
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
    /// The index could not be read or written while normalizing.
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
// External assembly identity
// ---------------------------------------------------------------------

/// The assembly a decompiled metadata file came from.
///
/// Roslyn writes a header before the decompiled source:
///
/// ```text
/// #region <label> System.Console, Version=10.0.0.0, Culture=neutral, PublicKeyToken=...
/// // /usr/local/share/dotnet/packs/.../System.Console.dll
/// #endregion
/// ```
///
/// Two things make this readable rather than fragile. The label is
/// **localized** -- it was measured on a Korean SDK, where it reads
/// `어셈블리` -- so the word is never matched; what is read is the text
/// after the `#region` token. And the second line is a machine path, so
/// it is skipped entirely: a `/Users/...` or `/usr/local/...` prefix must
/// never reach canonical identity.
///
/// Opening the file is allowed for exactly this. Nothing from it is
/// persisted as source, indexed, or prepared for an Agent.
#[must_use]
pub fn assembly_identity(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let line = text.lines().next()?;
    // Strip a BOM, then the `#region` token, then the localized label.
    let line = line.trim_start_matches('\u{feff}').trim_start();
    let rest = line.strip_prefix("#region")?.trim_start();
    // The identity is `Name, Version=…, Culture=…, PublicKeyToken=…`.
    // The label is whatever precedes it, so the identity starts at the
    // last whitespace-separated run before the first comma.
    let (before_comma, _) = rest.split_once(',')?;
    let name = before_comma.split_whitespace().last()?;
    let assembly = rest.get(rest.find(name)?..)?.trim();
    (!assembly.is_empty()).then(|| assembly.to_owned())
}

/// A referenced assembly's type or member, as canonical identity.
///
/// The assembly identity comes from the header; the type name is the
/// decompiled file's own name, which Roslyn derives from the type; the
/// member is the name the referring site wrote. No machine path, no
/// content hash, no temp directory.
#[must_use]
pub fn external_entity(path: &Path, written_name: Option<&str>) -> Option<ExternalEntity> {
    let assembly = assembly_identity(path)?;
    let package_identity = assembly
        .split(',')
        .next()
        .map(str::trim)
        .filter(|name| !name.is_empty())?
        .to_owned();
    let type_name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty());
    let symbol_name = written_name
        .and_then(|name| name.rsplit('.').next())
        .filter(|name| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|character| character.is_alphanumeric() || character == '_')
        })
        .map(str::to_owned);
    Some(ExternalEntity {
        package_identity,
        qualified_name: match (&type_name, &symbol_name) {
            (Some(type_name), Some(symbol)) => Some(format!("{type_name}.{symbol}")),
            (Some(type_name), None) => Some(type_name.clone()),
            (None, symbol) => symbol.clone(),
        },
        kind: if symbol_name.is_some() {
            EXTERNAL_SYMBOL.to_owned()
        } else {
            EXTERNAL_ASSEMBLY.to_owned()
        },
        module_path: type_name,
        symbol_name,
        // The full `Version=…, Culture=…, PublicKeyToken=…` the header
        // states. Kept because a different version of one assembly is a
        // different set of APIs.
        resolved_version: Some(assembly),
        // A machine path is a locator, never identity.
        declaration_locator: None,
    })
}

// ---------------------------------------------------------------------
// Partial declarations
// ---------------------------------------------------------------------

/// The kinds a `partial` declaration can declare.
///
/// Types only, deliberately. A `partial` *method* has one implementing
/// declaration, so a definition resolves to that one and there is
/// nothing to group -- and the fixture holds a trap that proves this
/// must be checked rather than assumed: `public override void Run()`
/// and `void IRunner.Run()` are two different members of `Runner` which
/// the structural tier records under the same qualified name. Grouping
/// them would merge an override with an explicit interface
/// implementation, which are not the same thing at all.
#[must_use]
pub const fn is_partial_type_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Interface | SymbolKind::Record
    )
}

/// Whether a declaration header writes `partial`.
///
/// Read from the modifier run before the declaring keyword, which is
/// exactly where C# allows modifiers. Nothing is matched by name.
///
/// This -- not "the backend returned more than one location" -- is what
/// makes a group legitimate. Two same-named types that are not `partial`
/// are two types and a genuine ambiguity, and stay candidates.
#[must_use]
pub fn declares_partial(signature: &str) -> bool {
    signature
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .any(|word| word == "partial")
}

/// How many type parameters a declaration header writes.
///
/// `Box<T>` and `Box` are different types, so arity is part of identity.
#[must_use]
pub fn declared_arity(signature: &str) -> usize {
    let Some(open) = signature.find('<') else {
        return 0;
    };
    let mut depth = 0_usize;
    let mut parameters = 1_usize;
    for character in signature[open..].chars() {
        match character {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return parameters;
                }
            }
            ',' if depth == 1 => parameters += 1,
            _ => {}
        }
    }
    0
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
}

/// Resolves backend locations against the current Workspace.
pub struct Normalizer<'a> {
    connection: &'a Connection,
    workspace_root: &'a Path,
    context_key: &'a str,
    /// The project each Resource belongs to, so a logical identity can
    /// name its compilation unit without re-discovering it per call.
    projects: &'a BTreeMap<ResourceId, String>,
    generation_id: i64,
    texts: BTreeMap<ResourceId, String>,
    read: BTreeMap<ResourceId, String>,
    /// Logical symbols this pass created or confirmed, with the
    /// declarations proved into them.
    grouped: BTreeMap<LogicalSymbolId, BTreeSet<SymbolId>>,
    /// What each resolved alias declaration binds to, by the line it is
    /// written on. See [`Normalizer::through_alias`].
    aliases: BTreeMap<(ResourceId, u32), GraphEndpoint>,
}

impl<'a> Normalizer<'a> {
    #[must_use]
    pub fn new(
        connection: &'a Connection,
        workspace_root: &'a Path,
        context_key: &'a str,
        projects: &'a BTreeMap<ResourceId, String>,
        generation_id: i64,
    ) -> Self {
        Self {
            connection,
            workspace_root,
            context_key,
            projects,
            generation_id,
            texts: BTreeMap::new(),
            read: BTreeMap::new(),
            grouped: BTreeMap::new(),
            aliases: BTreeMap::new(),
        }
    }

    /// Record what an alias declaration was proved to bind to.
    ///
    /// `using Alias = Contracts.Model;` declares a name, and the
    /// measured server answers a *use* of that name with the alias
    /// declaration itself -- line 3, the word `Alias` -- which is not a
    /// Symbol and never will be. The type it stands for is the answer
    /// to a different question the same pass already asked: the
    /// directive's own right-hand side, `Contracts.Model`, which
    /// resolves to the type.
    ///
    /// So an alias use is resolved by chaining those two answers. Both
    /// links are the compiler's; nothing here splits a dotted name or
    /// matches by text. The join is positional -- a C# `using`
    /// directive is written on one line -- and an alias written across
    /// several lines simply stays a gap rather than resolving to
    /// something else.
    fn remember_alias(&mut self, resource: ResourceId, line: u32, target: GraphEndpoint) {
        self.aliases.insert((resource, line), target);
    }

    /// The type an alias declaration at this location stands for.
    fn through_alias(&self, resource: ResourceId, line: u32) -> Option<GraphEndpoint> {
        self.aliases.get(&(resource, line)).cloned()
    }

    /// Every Resource whose current bytes a resolution depended on.
    #[must_use]
    pub const fn sources_read(&self) -> &BTreeMap<ResourceId, String> {
        &self.read
    }

    /// The groups this pass proved, and what went into them.
    #[must_use]
    pub const fn grouped(&self) -> &BTreeMap<LogicalSymbolId, BTreeSet<SymbolId>> {
        &self.grouped
    }

    fn resource_for_uri(&self, uri: &str) -> Result<Option<Resource>, AdapterError> {
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

    /// The Symbol one backend location names, if it names one.
    fn symbol_for(&mut self, location: &Location) -> Result<Option<SymbolId>, AdapterError> {
        let Some(resource) = self.resource_for_uri(&location.uri)? else {
            return Ok(None);
        };
        let Some(text) = self.text_of(&resource) else {
            return Ok(None);
        };
        let Ok((start, end)) = LineMap::with_encoding(text, POSITION_ENCODING).span(location.range)
        else {
            return Ok(None);
        };
        match symbol_at_span(self.connection, resource.id, start, end).map_err(index)? {
            SymbolMatch::One(symbol) => Ok(Some(symbol)),
            SymbolMatch::Ambiguous | SymbolMatch::None => Ok(None),
        }
    }

    /// Turn one backend location into a Brainprint endpoint.
    fn target_for(
        &mut self,
        location: &Location,
        written_name: Option<&str>,
    ) -> Result<Target, AdapterError> {
        // Decompiled metadata: not Workspace source, but a real
        // assembly. The path is a content hash and is never identity.
        if is_metadata_uri(&location.uri) {
            return Ok(self.metadata_target(&location.uri, written_name));
        }
        let Some(resource) = self.resource_for_uri(&location.uri)? else {
            return Ok(Target::Unmappable(UnresolvedReason::NoStructuralBinding));
        };
        if location.range.is_empty() {
            return Ok(Target::Endpoint(GraphEndpoint::Resource(resource.id)));
        }
        match self.symbol_for(location)? {
            Some(symbol) => Ok(Target::Endpoint(GraphEndpoint::Symbol(symbol))),
            None => Ok(self
                .through_alias(resource.id, location.range.start.line)
                .map_or(
                    Target::Unmappable(UnresolvedReason::NoStructuralBinding),
                    Target::Endpoint,
                )),
        }
    }

    fn metadata_target(&self, uri: &str, written_name: Option<&str>) -> Target {
        let Some(path) = uri_to_path(uri) else {
            return Target::Unmappable(UnresolvedReason::NoStructuralBinding);
        };
        external_entity(&path, written_name).map_or(
            Target::Unmappable(UnresolvedReason::NoStructuralBinding),
            |entity| Target::Endpoint(GraphEndpoint::External(entity)),
        )
    }

    /// Group a set of declarations into one logical symbol.
    ///
    /// The identity is derived from the *declarations*, never from which
    /// of them the backend happened to list first: they all agree on
    /// qualified name, kind and arity, because they are declarations of
    /// one type.
    fn group(
        &mut self,
        declarations: &[SymbolId],
    ) -> Result<Option<LogicalSymbolId>, AdapterError> {
        let Some(first) = declarations.first() else {
            return Ok(None);
        };
        let Some(descriptor) = describe(self.connection, *first).map_err(index)? else {
            return Ok(None);
        };
        let project_key = self
            .projects
            .get(&descriptor.resource)
            .cloned()
            .unwrap_or_default();
        let identity = LogicalIdentity {
            context_key: self.context_key.to_owned(),
            project_key,
            qualified_name: descriptor.qualified_name,
            kind: descriptor.kind,
            arity: descriptor.arity,
            discriminator: String::new(),
        };
        let logical = logical_symbol::ensure(self.connection, &identity, self.generation_id)
            .map_err(index)?;
        let members = self.grouped.entry(logical).or_default();
        for declaration in declarations {
            logical_symbol::declare(
                self.connection,
                logical,
                *declaration,
                self.context_key,
                self.generation_id,
            )
            .map_err(index)?;
            members.insert(*declaration);
        }
        Ok(Some(logical))
    }
}

/// What one Symbol row says about itself, for identity purposes.
struct Descriptor {
    resource: ResourceId,
    qualified_name: String,
    kind: SymbolKind,
    arity: usize,
    /// Whether the declaration header writes `partial`.
    partial: bool,
}

fn describe(
    connection: &Connection,
    symbol: SymbolId,
) -> Result<Option<Descriptor>, rusqlite::Error> {
    use rusqlite::{OptionalExtension, params};
    let row: Option<(Vec<u8>, String, String, String, i64, i64)> = connection
        .query_row(
            "SELECT resource.uid, symbol.qualified_name, symbol.kind, \
                    COALESCE(symbol.signature, ''), symbol.start_byte, symbol.end_byte \
             FROM symbol JOIN resource ON resource.id = symbol.resource_id \
             WHERE symbol.uid = ?1",
            params![symbol.to_bytes().to_vec()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((uid, qualified_name, kind, signature, _, _)) = row else {
        return Ok(None);
    };
    let bytes: [u8; 16] = uid
        .as_slice()
        .try_into()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    Ok(Some(Descriptor {
        resource: ResourceId::from_bytes(bytes),
        qualified_name,
        kind: SymbolKind::parse_public(&kind).unwrap_or(SymbolKind::Class),
        // The stored signature is the declaration header, which is
        // where both the modifiers and a type-parameter list live.
        arity: declared_arity(&signature),
        partial: declares_partial(&signature),
    }))
}

// ---------------------------------------------------------------------
// Resolving one Resource
// ---------------------------------------------------------------------

/// Ask once, and ask again if the backend withdrew the request.
///
/// `Ok(None)` means it withdrew both times. One retry rather than a loop:
/// the server cancels while it re-analyses a document, which settles,
/// and a caller that retried forever would hang instead of reporting
/// incomplete coverage.
fn ask(
    queries: &dyn CSharpQueries,
    request: &CSharpRequest,
) -> Result<Option<Vec<Location>>, AdapterError> {
    for _ in 0..2 {
        match queries.call(request)? {
            CSharpResponse::Locations(locations) => return Ok(Some(locations)),
            CSharpResponse::Cancelled(_) => {}
            CSharpResponse::Unsupported(method) => {
                return Err(AdapterError::Protocol(format!("{method} is unsupported")));
            }
            other => return Err(AdapterError::Protocol(format!("{other:?}"))),
        }
    }
    Ok(None)
}

/// What one Resource's semantic pass produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceEvidence {
    pub evidence: Vec<SemanticEvidence>,
    /// Sites the backend withdrew rather than answered. Incomplete
    /// coverage, recorded rather than guessed at.
    pub withdrawn: Vec<crate::evidence::OccurrenceRef>,
    /// `(type, base)` pairs this pass proved, so the override and
    /// interface derivations can use an edge that has not merged yet.
    pub resolved_bases: Vec<(SymbolId, SymbolId)>,
    pub retained_external: Vec<RetainedExternal>,
    pub deferred: Vec<DeferredGap>,
    /// Logical symbols proved, with their declarations.
    pub grouped: BTreeMap<LogicalSymbolId, BTreeSet<SymbolId>>,
}

/// One Resource's inputs for a semantic pass.
pub struct ResourceRequest<'a> {
    pub owner: &'a Resource,
    pub owner_text: &'a str,
    pub sites: &'a SiteSet,
    pub occurrences: &'a [Occurrence],
    pub generation_id: i64,
    pub analysis_profile_id: i64,
    pub context_key: &'a str,
}

/// How this backend reads a shared [`ResolvableKind`].
trait CSharpSite {
    fn capability(self, outcome: &SemanticOutcome) -> SemanticCapability;
    fn dispatch(self, connection: &Connection, outcome: &SemanticOutcome) -> Dispatch;
}

impl CSharpSite for ResolvableKind {
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
            Self::Extends => SemanticCapability::Inheritance,
            Self::Implements => SemanticCapability::Implements,
        }
    }

    /// C# binds a call to a declaration; what runs may be an override.
    ///
    /// The measurement that makes this worth stating: `BaseRunner runner
    /// = new Runner(); runner.Run();` resolved to `BaseRunner.Run`, the
    /// declaration the static type names -- not to `Runner.Run`, which
    /// is what executes. Calling that STATIC would turn "here is the
    /// declaration" into "here is what runs", which C# does not support
    /// anyone saying.
    ///
    /// So dispatch is read off the resolved declaration, which is the
    /// only place the answer is written: a `virtual`, `abstract` or
    /// `override` member, or any member of an interface, can be
    /// dispatched somewhere else at run time. Everything else in C# --
    /// a static method, a sealed override, an ordinary non-virtual
    /// method, a type reference -- is bound exactly where it points.
    /// `Overloads.Parse("x")` is not made unknowable by the fact that
    /// some other call in the file is.
    fn dispatch(self, connection: &Connection, outcome: &SemanticOutcome) -> Dispatch {
        if self != Self::Calls {
            return Dispatch::Static;
        }
        let SemanticOutcome::Resolved {
            target: GraphEndpoint::Symbol(target),
        } = outcome
        else {
            return Dispatch::Static;
        };
        if overridable_target(connection, *target).unwrap_or(true) {
            Dispatch::Unknown
        } else {
            Dispatch::Static
        }
    }
}

/// EXTENDS or IMPLEMENTS, once the target's own kind is known.
///
/// A base list does not say which it is -- `class Runner : BaseRunner,
/// IRunner` writes both the same way -- so the answer is the *target's*
/// declared kind. I3 gets this right whenever the target is in the same
/// file, and cannot when it is not: an unresolved base defaults to
/// EXTENDS because something has to be recorded. Correcting it is what
/// this tier is for, and it is a correction rather than a guess,
/// because by now the compiler has said which declaration it is.
fn base_list_kind(
    connection: &Connection,
    kind: ResolvableKind,
    outcome: &SemanticOutcome,
) -> crate::graph::RelationKind {
    use crate::graph::RelationKind;
    if !matches!(kind, ResolvableKind::Extends | ResolvableKind::Implements) {
        return kind.relation_kind();
    }
    let target = match outcome {
        SemanticOutcome::Resolved {
            target: GraphEndpoint::Symbol(symbol),
        } => Some(*symbol),
        // Every declaration of a partial type agrees on its kind, so
        // reading one answers for the group.
        SemanticOutcome::Resolved {
            target: GraphEndpoint::Logical(logical),
        } => crate::logical_symbol::declarations(connection, *logical)
            .ok()
            .and_then(|declarations| declarations.into_iter().next()),
        _ => None,
    };
    let Some(target) = target else {
        return kind.relation_kind();
    };
    match describe(connection, target) {
        Ok(Some(descriptor)) if descriptor.kind == SymbolKind::Interface => {
            RelationKind::Implements
        }
        Ok(Some(_)) => RelationKind::Extends,
        _ => kind.relation_kind(),
    }
}

/// Whether a resolved callee could be dispatched to something else.
///
/// True for `virtual`, `abstract` and non-`sealed` `override` members,
/// and for every member an interface declares. `Err` is treated as
/// unknown by the caller, because failing to read a declaration is not
/// evidence that it is final.
fn overridable_target(connection: &Connection, target: SymbolId) -> Result<bool, rusqlite::Error> {
    use rusqlite::{OptionalExtension, params};
    let row: Option<(String, Option<String>)> = connection
        .query_row(
            "SELECT COALESCE(symbol.signature, ''), parent.kind \
             FROM symbol LEFT JOIN symbol AS parent ON parent.id = symbol.parent_symbol_id \
             WHERE symbol.uid = ?1",
            params![target.to_bytes().to_vec()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((signature, parent_kind)) = row else {
        return Ok(true);
    };
    if parent_kind.as_deref() == Some("INTERFACE") {
        return Ok(true);
    }
    let words: Vec<&str> = signature
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .collect();
    let sealed = words.contains(&"sealed");
    Ok(!sealed
        && (words.contains(&"virtual")
            || words.contains(&"abstract")
            || words.contains(&"override")))
}

/// Ask the backend about every site this Resource offers, and normalize
/// the answers.
///
/// # Errors
/// When the backend cannot be reached or answers a shape this build does
/// not read.
pub fn resolve_resource(
    queries: &dyn CSharpQueries,
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
        let question = CSharpRequest::Definition {
            uri: owner_uri.clone(),
            position,
        };
        let locations = match ask(queries, &question)? {
            Some(locations) => locations,
            // Withdrawn twice. That is incomplete coverage, not an
            // answer and not a dead backend: the site stays the gap it
            // already was, and the rest of the Resource still resolves.
            None => {
                produced.withdrawn.push(site.occurrence);
                continue;
            }
        };

        let name = (!site.lookup_name.is_empty()).then_some(site.lookup_name.clone());
        let outcome = normalize(normalizer, &locations, name.as_deref(), site)?;

        // An `import` site that resolved is either an ordinary import or
        // the right-hand side of an alias declaration. Either way it is
        // what a use of the name written on this line stands for.
        if site.kind == ResolvableKind::Imports
            && let SemanticOutcome::Resolved { target } = &outcome
        {
            let line = map
                .range(site.occurrence.start_byte, site.occurrence.end_byte)
                .map(|range| range.start.line)
                .unwrap_or_default();
            normalizer.remember_alias(request.owner.id, line, target.clone());
        }
        let source = source_endpoint(request.owner.id, request.occurrences, site.occurrence);
        let dispatch = site.kind.dispatch(normalizer.connection, &outcome);
        let relation_kind = base_list_kind(normalizer.connection, site.kind, &outcome);

        if matches!(
            site.kind,
            ResolvableKind::Extends | ResolvableKind::Implements
        ) && let SemanticOutcome::Resolved {
            target: GraphEndpoint::Symbol(base),
        } = &outcome
            && let GraphEndpoint::Symbol(declaring) = source
        {
            produced.resolved_bases.push((declaring, *base));
        }

        produced.evidence.push(SemanticEvidence {
            context_key: request.context_key.to_owned(),
            capability: if relation_kind == crate::graph::RelationKind::Implements
                && site.kind == ResolvableKind::Extends
            {
                SemanticCapability::Implements
            } else {
                site.kind.capability(&outcome)
            },
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

    produced.grouped = normalizer.grouped().clone();
    Ok(produced)
}

/// One answer set becomes one outcome.
///
/// The partial-type rule lives here, and it is the whole reason this
/// function is not three lines. Several Workspace declarations that are
/// all declarations of one type are **not** candidates -- the compiler
/// settled it -- so they become one binding to the logical symbol that
/// owns them. Several declarations that are *not* one type stay
/// candidates, because then the backend really did not settle it.
fn normalize(
    normalizer: &mut Normalizer<'_>,
    locations: &[Location],
    written_name: Option<&str>,
    site: &SemanticSite,
) -> Result<SemanticOutcome, AdapterError> {
    let mut endpoints: Vec<GraphEndpoint> = Vec::new();
    let mut declarations: Vec<SymbolId> = Vec::new();
    let mut unmappable: Option<UnresolvedReason> = None;

    for location in locations {
        match normalizer.target_for(location, written_name)? {
            Target::Endpoint(GraphEndpoint::Symbol(symbol)) => {
                if !declarations.contains(&symbol) {
                    declarations.push(symbol);
                }
            }
            Target::Endpoint(endpoint) => {
                if !endpoints.contains(&endpoint) {
                    endpoints.push(endpoint);
                }
            }
            Target::Unmappable(reason) => unmappable = unmappable.or(Some(reason)),
        }
    }

    if declarations.len() > 1 && same_type(normalizer.connection, &declarations) {
        // One type, several declarations. One binding.
        if let Some(logical) = normalizer.group(&declarations)? {
            endpoints.push(GraphEndpoint::Logical(logical));
            declarations.clear();
        }
    }
    endpoints.extend(declarations.into_iter().map(GraphEndpoint::Symbol));

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

/// Whether every declaration is a `partial` declaration of one type.
///
/// Four things have to hold, and each one rules out a real case that
/// would otherwise be grouped wrongly: every declaration writes
/// `partial`, so two same-named non-partial types stay the genuine
/// ambiguity they are; every kind is a type kind, so the fixture's
/// `Run` override and its explicit `IRunner.Run` implementation are not
/// merged by their shared qualified name; and name, kind and arity all
/// agree, so a `Box` and a `Box<T>` stay two types.
///
/// These are the same fields [`LogicalIdentity`] keys on, checked before
/// anything is created rather than discovered afterwards.
fn same_type(connection: &Connection, declarations: &[SymbolId]) -> bool {
    let mut descriptors = declarations
        .iter()
        .map(|symbol| describe(connection, *symbol).ok().flatten());
    let Some(Some(first)) = descriptors.next() else {
        return false;
    };
    if !first.partial || !is_partial_type_kind(first.kind) {
        return false;
    }
    descriptors.all(|descriptor| {
        descriptor.is_some_and(|other| {
            other.partial
                && other.qualified_name == first.qualified_name
                && other.kind == first.kind
                && other.arity == first.arity
        })
    })
}

// ---------------------------------------------------------------------
// Synchronization
// ---------------------------------------------------------------------

/// Hand the backend Brainprint's current bytes for one Resource.
///
/// The measured requirement: a watched-file notification alone left the
/// server answering the old position, and this did not. The bytes are
/// the ones the Workspace indexed -- there is no editor here and no
/// unsaved buffer to accept.
///
/// # Errors
/// When the notification could not be delivered.
pub fn open_document(
    queries: &dyn CSharpQueries,
    uri: String,
    text: String,
    version: i64,
) -> Result<(), RequestFailure> {
    synchronize_document(queries, uri, text, version, None)
}

/// The same, choosing `didOpen` or `didChange` by what the connection
/// already holds.
///
/// LSP allows one `didOpen` per document. The live server answered a
/// second one by cancelling the requests already in flight, so which of
/// the two is sent is not a style choice.
///
/// # Errors
/// When the notification could not be delivered.
pub fn synchronize_document(
    queries: &dyn CSharpQueries,
    uri: String,
    text: String,
    version: i64,
    previous: Option<&str>,
) -> Result<(), RequestFailure> {
    let request = match previous {
        Some(previous) => CSharpRequest::ChangeDocument {
            uri,
            text,
            version,
            replaces: whole_of(previous),
        },
        None => CSharpRequest::OpenDocument { uri, text, version },
    };
    queries.call(&request).map(|_| ())
}

/// The range covering an entire document.
///
/// What a full replacement looks like to a server that declares
/// incremental sync.
#[must_use]
pub fn whole_of(text: &str) -> crate::lsp::coordinates::Range {
    LineMap::with_encoding(text, POSITION_ENCODING)
        .range(0, text.len())
        .unwrap_or_else(|_| {
            crate::lsp::coordinates::Range::new(
                crate::lsp::coordinates::Position::new(0, 0),
                crate::lsp::coordinates::Position::new(0, 0),
            )
        })
}

/// Tell the backend the filesystem moved.
///
/// Not sufficient on its own for an edit -- see [`open_document`] -- but
/// it is what a project-membership change is announced with before a
/// reload.
///
/// # Errors
/// When the notification could not be delivered.
pub fn notify_watched_files(
    queries: &dyn CSharpQueries,
    changes: Vec<super::protocol::WatchedChange>,
) -> Result<(), RequestFailure> {
    queries
        .call(&CSharpRequest::WatchedFilesChanged { changes })
        .map(|_| ())
}

/// Ask the backend to load a project set.
///
/// Loading a project executes its build logic, so this is only ever
/// reached through
/// [`lifecycle::reload_projects`](super::lifecycle::reload_projects),
/// which refuses it outright without
/// [`ProjectExecutionTrust::Trusted`](super::protocol::ProjectExecutionTrust::Trusted).
///
/// # Errors
/// When the request could not be delivered.
pub fn reopen_projects(
    queries: &dyn CSharpQueries,
    uris: Vec<String>,
) -> Result<(), RequestFailure> {
    queries
        .call(&CSharpRequest::OpenProjects { uris })
        .map(|_| ())
}

// ---------------------------------------------------------------------
// Declaration claims
// ---------------------------------------------------------------------

/// Which members of `owner` are written `override`.
///
/// Syntax only, and validation evidence only: it never creates an edge.
/// A claim whose base member cannot be proven is reported as an honest
/// gap by [`crate::semantic_overrides`]; an edge still needs proven
/// inheritance. C# requires the keyword, so unlike TypeScript a member
/// without it is not an override -- but that is the *language's*
/// guarantee, not this function's, and the derivation still only
/// follows proven edges.
#[must_use]
pub fn override_members(
    owner_text: &str,
    symbols: &[Symbol],
    occurrences: &[Occurrence],
) -> BTreeSet<SymbolId> {
    crate::typescript_semantic::adapter::override_keyword_members(owner_text, symbols, occurrences)
}

/// The other Resources that declare the same types as `symbols` do.
///
/// A partial type's declarations, minus the ones already in hand. What
/// a claim written in one half has to be read from, to be honoured in
/// the other.
///
/// # Errors
/// When the index cannot be read.
pub fn sibling_resources(
    connection: &Connection,
    owner: ResourceId,
    symbols: &[Symbol],
) -> Result<BTreeSet<ResourceId>, AdapterError> {
    let mut found = BTreeSet::new();
    for symbol in symbols {
        for group in crate::logical_symbol::groups_of(connection, symbol.id).map_err(index)? {
            for declaration in
                crate::logical_symbol::declarations(connection, group).map_err(index)?
            {
                if let Some(descriptor) = describe(connection, declaration).map_err(index)?
                    && descriptor.resource != owner
                {
                    found.insert(descriptor.resource);
                }
            }
        }
    }
    Ok(found)
}

/// Which members of `owner` explicitly implement a named interface, and
/// which interface each names.
///
/// `void IRunner.Run() { }` is C#'s explicit form, and the qualifier is
/// part of the declaration rather than a modifier: it sits between the
/// return type and the member name. So the interface is read from the
/// text immediately before the name -- `IRunner.` -- and nothing else
/// is consulted.
///
/// This matters twice over. The explicit member implements exactly the
/// interface it names. And it takes the name: a sibling `Run` that
/// looks like an implementation of `IRunner.Run` is not one, because
/// interface dispatch reaches the explicit declaration instead.
#[must_use]
pub fn explicit_interface_members(
    owner_text: &str,
    symbols: &[Symbol],
    occurrences: &[Occurrence],
) -> BTreeMap<SymbolId, String> {
    let mut found = BTreeMap::new();
    for member in symbols.iter().filter(|symbol| {
        matches!(
            symbol.kind,
            SymbolKind::Method | SymbolKind::Property | SymbolKind::Field
        )
    }) {
        let Some(name_site) = occurrences.iter().find(|occurrence| {
            occurrence.kind == crate::symbol::OccurrenceKind::Definition
                && occurrence.containing_symbol_id == Some(member.id)
        }) else {
            continue;
        };
        let Some(before) = owner_text.get(member.span.start_byte..name_site.span.start_byte) else {
            continue;
        };
        // The qualifier is the identifier immediately before the dot
        // that immediately precedes the name.
        let Some(qualifier) = before.strip_suffix('.') else {
            continue;
        };
        let interface: String = qualifier
            .chars()
            .rev()
            .take_while(|character| character.is_alphanumeric() || *character == '_')
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        if !interface.is_empty() {
            found.insert(member.id, interface);
        }
    }
    found
}
