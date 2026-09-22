//! Canonical Symbol and Occurrence model, `analysis_profile` reuse, and
//! the Resource-owned replacement primitive (#16 task 8-9 / #13 task 6
//! §7-8).
//!
//! Scope: this module owns the typed boundary over the `symbol`,
//! `occurrence`, and `analysis_profile` columns #15 task 5's schema
//! already created. It defines no new column, adds no migration, and
//! redesigns none. It does not parse -- turning a parse tree into
//! candidates is [`crate::extract`] -- and it knows nothing about Relation
//! (I3) or semantic resolution (I4).
//!
//! ## What a Symbol is here
//!
//! A *declared*, named thing worth finding again: the P0 taxonomy in
//! [`SymbolKind`]. Not every binding -- a local variable or an expression
//! is never a persistent Symbol (#16 task 8, #13 task 6 §7).
//!
//! `qualified_name` is a **structural lexical identity** within one
//! Resource: the chain of enclosing declarations and lexical segments that
//! contains it, joined with the language's own separator. It is not a
//! module path, and no import, package, or type resolution is inferred to
//! build it -- that is I4's work.
//!
//! The span is byte offsets plus line/column, so a declaration can be read
//! straight out of the current file. The source itself is never stored: a
//! `signature` is the compact declaration header only, never a body.
//!
//! ## Resource-owned replacement
//!
//! A Resource's Symbol set is replaced whole, in one transaction, or not at
//! all ([`SymbolStore::replace_for_resource`]). There is no row-by-row
//! update path, because a half-applied extraction is a Symbol set that
//! describes no version of the file. Immediately before writing, the
//! Resource is re-checked: still ACTIVE, and still at the
//! `resource_revision` the extraction was based on. A mismatch keeps the
//! existing set exactly as it is.
//!
//! [`SymbolStore::replace_structure`] extends that to the whole structural
//! result: Symbols and the [`Occurrence`] evidence about them are replaced
//! in one transaction, so no state exists in which one was rewritten and
//! the other was not. [`SymbolStore::replace_in_transaction`] and
//! [`SymbolStore::replace_occurrences_in_transaction`] are the same
//! primitives without the transaction, for a caller that owns one -- which
//! is also how I3 will be able to attach Relation evidence to the same
//! commit.
//!
//! ## Occurrence is evidence, not Relation
//!
//! An [`Occurrence`] records that a span of source *is* a definition, an
//! import site, or a call site. It names no target. `relation_id` and
//! `resolution_context_id` are left NULL at this tier, and deciding what a
//! call reaches or what an import names is I3/I4's work.

use std::{collections::HashMap, error::Error, fmt, path::Path};

use brainprint_core::{ResourceId, SymbolId};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::{
    db::{self, DbOpenError},
    generation::{self, GenerationError, PublicationGrant},
    parser::{ParserDescriptor, SourcePoint, SourceSpan, StructuralCapability},
    resource::{ResourceError, ResourceLanguage, ResourceState},
    schema,
    semantic::{AnalysisContext, CapabilityReport},
};

/// Version of the extraction semantics itself -- what this crate decides
/// counts as a Symbol, how it names it, and how it shapes a signature.
///
/// It is part of the profile identity so that results produced by
/// different extractor semantics are never silently compared as equal.
/// Bump it when the extraction rules change meaning.
pub const EXTRACTOR_SEMANTICS_VERSION: &str = "1";

/// No framework/container adapter participates in structural extraction
/// yet (#16 task 8 excludes Svelte embedded mapping and every framework
/// adapter). Recorded explicitly rather than left to look like a version.
pub const ADAPTER_SEMANTICS_VERSION: &str = "0";

/// What a Symbol row was produced by. Structural only at this tier.
pub const ANALYSIS_MODE_STRUCTURAL: &str = "STRUCTURAL";

/// What a normalized semantic result was produced by (#19 task 3). A
/// separate profile row from the structural one covering the same
/// language: the two are produced by different backends, carry different
/// capabilities, and go stale for different reasons.
pub const ANALYSIS_MODE_SEMANTIC: &str = "SEMANTIC";

/// Written where a profile genuinely has no value for a NOT NULL backend
/// column -- a semantic profile names no structural backend, and naming
/// tree-sitter there would claim a parse that never happened.
pub const BACKEND_NOT_APPLICABLE: &str = "NOT_APPLICABLE";

/// The P0 persistent Symbol taxonomy (#16 "persistent Symbol P0").
///
/// Closed on purpose. A declaration a supported language really has and
/// this list really lacks gets its own variant -- a Rust `trait` and a C#
/// `record` are not classes, and mapping them onto one would make the
/// index lie. A declaration outside the taxonomy is simply not extracted;
/// it is never squeezed into the nearest-looking kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Class,
    Interface,
    Struct,
    Enum,
    /// Rust `trait`. Neither a class nor an interface row.
    Trait,
    /// C# `record`/`record struct`.
    Record,
    Function,
    Method,
    TypeAlias,
    /// A declared accessor-backed member (C# property, TS interface
    /// property signature).
    Field,
    Property,
    /// A named, non-local constant binding: a module/class-level binding
    /// or an explicit `const`/`static`. Never a local variable.
    Constant,
}

impl SymbolKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Class => "CLASS",
            Self::Interface => "INTERFACE",
            Self::Struct => "STRUCT",
            Self::Enum => "ENUM",
            Self::Trait => "TRAIT",
            Self::Record => "RECORD",
            Self::Function => "FUNCTION",
            Self::Method => "METHOD",
            Self::TypeAlias => "TYPE_ALIAS",
            Self::Field => "FIELD",
            Self::Property => "PROPERTY",
            Self::Constant => "CONSTANT",
        }
    }

    /// Decode a stored kind. Public so a caller reading Symbol rows
    /// through another table shares this vocabulary rather than
    /// re-deriving it (#19 task 12).
    pub fn parse_public(raw: &str) -> Result<Self, SymbolError> {
        Self::parse(raw)
    }

    fn parse(raw: &str) -> Result<Self, SymbolError> {
        match raw {
            "CLASS" => Ok(Self::Class),
            "INTERFACE" => Ok(Self::Interface),
            "STRUCT" => Ok(Self::Struct),
            "ENUM" => Ok(Self::Enum),
            "TRAIT" => Ok(Self::Trait),
            "RECORD" => Ok(Self::Record),
            "FUNCTION" => Ok(Self::Function),
            "METHOD" => Ok(Self::Method),
            "TYPE_ALIAS" => Ok(Self::TypeAlias),
            "FIELD" => Ok(Self::Field),
            "PROPERTY" => Ok(Self::Property),
            "CONSTANT" => Ok(Self::Constant),
            other => Err(SymbolError::UnknownSymbolKind {
                raw: other.to_owned(),
            }),
        }
    }

    /// Whether a declaration of this kind opens a scope that its members
    /// belong to -- the difference between a method and a free function.
    #[must_use]
    pub const fn is_type_like(self) -> bool {
        matches!(
            self,
            Self::Class | Self::Interface | Self::Struct | Self::Enum | Self::Trait | Self::Record
        )
    }
}

impl fmt::Display for SymbolKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Declared visibility, as written in the source.
///
/// [`Self::Unspecified`] is a real answer, not a default to fall back on:
/// a Python declaration carries no visibility keyword, and inventing one
/// from a naming convention would be a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Private,
    Protected,
    /// Rust `pub(crate)`, C# `internal`.
    Internal,
    Unspecified,
}

impl Visibility {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "PUBLIC",
            Self::Private => "PRIVATE",
            Self::Protected => "PROTECTED",
            Self::Internal => "INTERNAL",
            Self::Unspecified => "UNSPECIFIED",
        }
    }

    fn parse(raw: &str) -> Result<Self, SymbolError> {
        match raw {
            "PUBLIC" => Ok(Self::Public),
            "PRIVATE" => Ok(Self::Private),
            "PROTECTED" => Ok(Self::Protected),
            "INTERNAL" => Ok(Self::Internal),
            "UNSPECIFIED" => Ok(Self::Unspecified),
            other => Err(SymbolError::UnknownVisibility {
                raw: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for Visibility {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One persisted declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub id: SymbolId,
    pub resource_id: ResourceId,
    /// The enclosing *Symbol*, when there is one. A block, an `if`, a
    /// loop, a C# namespace, or a Rust `impl` is not a Symbol, so nothing
    /// synthetic is invented to stand in for it -- those only contribute
    /// to [`Self::qualified_name`].
    pub parent_id: Option<SymbolId>,
    pub kind: SymbolKind,
    pub name: String,
    /// Lexical identity inside this Resource. Not a module path.
    pub qualified_name: String,
    /// The compact declaration header, never a body.
    pub signature: Option<String>,
    pub visibility: Visibility,
    /// Whether the declaration carries an explicit export marker of its
    /// own (`export`, `pub`, `public`). A language without such a marker
    /// reports `false` rather than a guess about reachability.
    pub exported: bool,
    pub span: SourceSpan,
    /// The `resource_revision` this row was extracted from.
    pub resource_revision: String,
    pub analysis_profile_id: i64,
}

/// What a span of source is evidence *of*.
///
/// Deliberately tiny. A structural parse can prove that a declaration
/// names itself here, that an import statement names something there, and
/// that a call is written at this position -- and that is all. Every other
/// identifier is left alone rather than asserted to be a REFERENCE to
/// something, because deciding what it refers to is resolution (I3/I4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OccurrenceKind {
    /// The name token of a declaration, at the declaration itself.
    Definition,
    /// The module or name an import/use statement writes. It is evidence
    /// that an import mentions this text -- not a resolved module entity.
    ImportSite,
    /// The callee of a call expression. It is evidence that a call is
    /// written here -- not a resolved target function.
    CallSite,
    /// A name used as a value rather than called: the `save` in
    /// `register(save)` (#17 task 5). Deliberately *not* an index of
    /// every identifier -- only the narrow shape that is a structural
    /// reference candidate, and never a declaration, an import, or a
    /// call's own callee, each of which already has its own evidence.
    ReferenceSite,
    /// A type named in a declaration: a base class, an implemented
    /// interface or trait, or an explicit parameter/return/field type
    /// (#17 task 6). Again a narrow set, not an index of every type
    /// token: only the positions whose relation the syntax settles.
    TypeSite,
    /// The key literal of a recognized environment or configuration
    /// access (#17 task 12): the `"DATABASE_URL"` in
    /// `os.getenv("DATABASE_URL")`, or the `DATABASE_URL` in
    /// `process.env.DATABASE_URL`. Only where the surrounding syntax
    /// establishes that the literal *is* the key -- a string that looks
    /// like one is not evidence.
    KeySite,
}

impl OccurrenceKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Definition => "DEFINITION",
            Self::ImportSite => "IMPORT_SITE",
            Self::CallSite => "CALL_SITE",
            Self::ReferenceSite => "REFERENCE_SITE",
            Self::TypeSite => "TYPE_SITE",
            Self::KeySite => "KEY_SITE",
        }
    }

    /// Decode a stored kind. Public so a caller reading Occurrence rows
    /// through another table (#17 task 7) shares this vocabulary rather
    /// than re-deriving it.
    pub fn parse_public(raw: &str) -> Result<Self, SymbolError> {
        Self::parse(raw)
    }

    fn parse(raw: &str) -> Result<Self, SymbolError> {
        match raw {
            "DEFINITION" => Ok(Self::Definition),
            "IMPORT_SITE" => Ok(Self::ImportSite),
            "CALL_SITE" => Ok(Self::CallSite),
            "REFERENCE_SITE" => Ok(Self::ReferenceSite),
            "TYPE_SITE" => Ok(Self::TypeSite),
            "KEY_SITE" => Ok(Self::KeySite),
            other => Err(SymbolError::UnknownOccurrenceKind {
                raw: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for OccurrenceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One piece of structural source evidence.
///
/// An Occurrence is **not** a Relation and carries no target. `relation_id`
/// and `resolution_context_id` exist in the schema for I3/I4 to fill in
/// later; everything written at this tier leaves both NULL, which is why
/// they are read-only here and never inputs.
///
/// It also has no stable identity: the row is evidence about a span of the
/// current source, so a re-extraction replaces a Resource's whole evidence
/// set rather than reconciling it row by row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    pub resource_id: ResourceId,
    /// The smallest Symbol that lexically contains this span, or `None`
    /// at file level. Lexical containment only -- no semantic owner is
    /// inferred.
    pub containing_symbol_id: Option<SymbolId>,
    pub kind: OccurrenceKind,
    /// The evidence's own narrow span, not the statement around it.
    pub span: SourceSpan,
    /// Always `None` at this tier: a structural Occurrence resolves
    /// nothing. Read back from storage so the invariant is checkable.
    pub relation_id: Option<i64>,
    pub analysis_profile_id: i64,
    /// Always `None` at this tier: there is no semantic resolution
    /// context to record.
    pub resolution_context_id: Option<i64>,
    pub resource_revision: String,
    /// The generation this evidence was published under, which must be
    /// the stable one at the time of writing.
    pub generation_id: i64,
}

/// The `analysis_profile` identity a structural extraction runs under.
///
/// Built from task 7's [`ParserDescriptor`] plus this crate's extractor
/// semantics. The **dialect is part of the identity**: a `.tsx` result and
/// a `.ts` result never share a profile, and neither do `.jsx` and `.js`
/// even though one grammar serves both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisProfile {
    pub profile_key: String,
    pub language: ResourceLanguage,
    pub analysis_mode: &'static str,
    pub structural_backend: &'static str,
    pub structural_backend_version: String,
    /// The semantic backend family, for a semantic profile. `None` for a
    /// structural one.
    pub semantic_backend: Option<String>,
    /// Recorded, and deliberately **not** part of [`Self::profile_key`]:
    /// a backend patch release is not by itself a different semantics,
    /// and making it one would turn every upgrade into a rebuild. What
    /// identifies comparability is
    /// [`Self::backend_compatibility_class`] (#19 task 3).
    pub semantic_backend_version: Option<String>,
    pub extractor_semantics_version: &'static str,
    pub adapter_semantics_version: &'static str,
    /// Which backend/dialect combination a result is comparable across.
    pub backend_compatibility_class: String,
    pub capability_fingerprint: String,
}

impl AnalysisProfile {
    /// The profile a semantic result for `context` belongs to (#19 task
    /// 3).
    ///
    /// Identity is language, backend family, analysis mode, semantics
    /// versions, compatibility class, and the declared capability set --
    /// everything that decides whether two results *mean* the same
    /// thing. The backend's own version is recorded beside it and left
    /// out of the key on purpose: upgrading a backend inside its
    /// compatibility class is not a change of meaning, and treating it
    /// as one would make every patch release a rebuild.
    #[must_use]
    pub fn semantic(context: &AnalysisContext, capabilities: &CapabilityReport) -> Self {
        let capability_fingerprint = capabilities.capability_fingerprint();
        let backend = context.backend.as_str();
        let backend_compatibility_class = context.toolchain.backend_compatibility_class.clone();

        let profile_key = db::fingerprint(
            "analysis-profile-1",
            &[
                ("language", &context.language.to_string()),
                ("analysis_mode", ANALYSIS_MODE_SEMANTIC),
                ("semantic_backend", backend),
                ("extractor_semantics", EXTRACTOR_SEMANTICS_VERSION),
                ("adapter_semantics", ADAPTER_SEMANTICS_VERSION),
                ("compatibility_class", &backend_compatibility_class),
                ("capability", &capability_fingerprint),
            ],
        );

        Self {
            profile_key,
            language: context.language,
            analysis_mode: ANALYSIS_MODE_SEMANTIC,
            structural_backend: BACKEND_NOT_APPLICABLE,
            structural_backend_version: BACKEND_NOT_APPLICABLE.to_owned(),
            semantic_backend: Some(backend.to_owned()),
            semantic_backend_version: Some(context.toolchain.backend_version.clone()),
            extractor_semantics_version: EXTRACTOR_SEMANTICS_VERSION,
            adapter_semantics_version: ADAPTER_SEMANTICS_VERSION,
            backend_compatibility_class,
            capability_fingerprint,
        }
    }

    /// The profile a parse under `descriptor` belongs to. Deterministic:
    /// the same descriptor always yields the same `profile_key`, which is
    /// what makes reuse possible instead of a row per extraction.
    #[must_use]
    pub fn of(descriptor: &ParserDescriptor) -> Self {
        let structural_backend_version = format!(
            "{}@abi{}+runtime-abi{}",
            descriptor.grammar, descriptor.grammar_abi_version, descriptor.backend_abi_version
        );
        // The grammar name alone would lose the JS/JSX distinction, since
        // one grammar serves both -- so the dialect is named outright.
        let backend_compatibility_class = format!("{}:{}", descriptor.backend, descriptor.dialect);
        let capability_fingerprint = match descriptor.capability {
            StructuralCapability::WholeFile => "whole-file".to_owned(),
            StructuralCapability::Container { embedded } => {
                let embedded: Vec<String> = embedded.iter().map(ToString::to_string).collect();
                format!("container:{}", embedded.join(","))
            }
        };

        let profile_key = db::fingerprint(
            "analysis-profile-1",
            &[
                ("language", &descriptor.language.to_string()),
                ("dialect", &descriptor.dialect.to_string()),
                ("analysis_mode", ANALYSIS_MODE_STRUCTURAL),
                ("structural_backend", descriptor.backend),
                ("structural_backend_version", &structural_backend_version),
                ("extractor_semantics", EXTRACTOR_SEMANTICS_VERSION),
                ("adapter_semantics", ADAPTER_SEMANTICS_VERSION),
                ("compatibility_class", &backend_compatibility_class),
                ("capability", &capability_fingerprint),
            ],
        );

        Self {
            profile_key,
            language: descriptor.language,
            analysis_mode: ANALYSIS_MODE_STRUCTURAL,
            structural_backend: descriptor.backend,
            structural_backend_version,
            semantic_backend: None,
            semantic_backend_version: None,
            extractor_semantics_version: EXTRACTOR_SEMANTICS_VERSION,
            adapter_semantics_version: ADAPTER_SEMANTICS_VERSION,
            backend_compatibility_class,
            capability_fingerprint,
        }
    }
}

/// Failure reading, decoding, or replacing Symbol rows.
#[derive(Debug)]
pub enum SymbolError {
    Open(DbOpenError),
    Sqlite(rusqlite::Error),
    Resource(ResourceError),
    Generation(GenerationError),
    UnknownSymbolKind {
        raw: String,
    },
    UnknownVisibility {
        raw: String,
    },
    UnknownOccurrenceKind {
        raw: String,
    },
    /// Evidence was offered against a generation that is not the current
    /// stable one -- building, aborted, or already superseded. Nothing is
    /// attached to it.
    GenerationNotStable {
        generation_id: i64,
        stable: Option<i64>,
    },
    /// A publication grant was offered for a generation begun against a
    /// different Workspace revision than the publication is establishing.
    PublicationBasisMismatch {
        generation_id: i64,
        basis: String,
        publication: String,
    },
    /// An Occurrence named a containing Symbol that is not part of the
    /// Symbol set it was published with.
    UnknownContainingSymbol {
        symbol: SymbolId,
    },
    /// No `resource` row for the id a replacement names.
    UnknownResource {
        resource_id: ResourceId,
    },
    /// The Resource is no longer ACTIVE, so nothing may be written for it.
    /// Its existing Symbol set is left alone.
    ResourceNotActive {
        resource_id: ResourceId,
        state: ResourceState,
    },
    /// The Resource moved on while the extraction was running: what was
    /// extracted describes a revision the file no longer has. The existing
    /// Symbol set is kept, and the caller may re-extract.
    RevisionMismatch {
        resource_id: ResourceId,
        basis: String,
        current: String,
    },
    /// A Symbol named a parent that is not in the same replacement. A
    /// Resource's Symbol set is self-contained by construction, so this is
    /// a bug in the extractor, not a race.
    UnknownParent {
        symbol: SymbolId,
        parent: SymbolId,
    },
}

impl SymbolError {
    /// Whether re-running the extraction could plausibly succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RevisionMismatch { .. })
    }
}

impl fmt::Display for SymbolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(source) => write!(formatter, "failed to open index.db: {source}"),
            Self::Sqlite(source) => write!(formatter, "symbol store sqlite error: {source}"),
            Self::Resource(source) => write!(formatter, "resource row failed to decode: {source}"),
            Self::Generation(source) => write!(formatter, "generation lookup failed: {source}"),
            Self::UnknownSymbolKind { raw } => write!(formatter, "unknown symbol kind {raw:?}"),
            Self::UnknownVisibility { raw } => write!(formatter, "unknown visibility {raw:?}"),
            Self::UnknownOccurrenceKind { raw } => {
                write!(formatter, "unknown occurrence kind {raw:?}")
            }
            Self::GenerationNotStable {
                generation_id,
                stable,
            } => write!(
                formatter,
                "generation {generation_id} is not the current stable generation ({}), so no \
                 evidence was attached to it",
                stable.map_or_else(|| "none".to_owned(), |id| id.to_string())
            ),
            Self::PublicationBasisMismatch {
                generation_id,
                basis,
                publication,
            } => write!(
                formatter,
                "generation {generation_id} was begun against workspace revision {basis},                  not the {publication} this publication establishes"
            ),
            Self::UnknownContainingSymbol { symbol } => write!(
                formatter,
                "occurrence names containing symbol {symbol}, which is not in the replacement"
            ),
            Self::UnknownResource { resource_id } => {
                write!(formatter, "no resource row for {resource_id}")
            }
            Self::ResourceNotActive { resource_id, state } => write!(
                formatter,
                "resource {resource_id} is {state}, so its symbols were not replaced"
            ),
            Self::RevisionMismatch {
                resource_id,
                basis,
                current,
            } => write!(
                formatter,
                "resource {resource_id} is at revision {current}, but the extraction was based \
                 on {basis}; the existing symbols were kept"
            ),
            Self::UnknownParent { symbol, parent } => write!(
                formatter,
                "symbol {symbol} names parent {parent}, which is not part of the replacement"
            ),
        }
    }
}

impl Error for SymbolError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Open(source) => Some(source),
            Self::Sqlite(source) => Some(source),
            Self::Resource(source) => Some(source),
            Self::Generation(source) => Some(source),
            _ => None,
        }
    }
}

impl From<DbOpenError> for SymbolError {
    fn from(source: DbOpenError) -> Self {
        Self::Open(source)
    }
}

impl From<ResourceError> for SymbolError {
    fn from(source: ResourceError) -> Self {
        Self::Resource(source)
    }
}

impl From<GenerationError> for SymbolError {
    fn from(source: GenerationError) -> Self {
        Self::Generation(source)
    }
}

impl From<rusqlite::Error> for SymbolError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

/// The `symbol` columns [`decode_symbol`] reads, in the order
/// [`raw_symbol_row`] expects. A caller may append further columns after
/// these -- their indices start where this list ends.
pub(crate) const SYMBOL_COLUMNS: &str = "s.uid, r.uid, p.uid, s.kind, s.name, s.qualified_name, s.signature, \
     s.visibility, s.exported, s.start_byte, s.end_byte, s.start_line, \
     s.start_col, s.end_line, s.end_col, s.resource_revision, s.analysis_profile_id";

/// The joins [`SYMBOL_COLUMNS`] needs: the owning Resource (`r`) and the
/// parent Symbol (`p`), if any.
pub(crate) const SYMBOL_FROM: &str = "FROM symbol s \
     JOIN resource r ON r.id = s.resource_id \
     LEFT JOIN symbol p ON p.id = s.parent_symbol_id";

/// Typed access to one Workspace's `symbol` and `analysis_profile` tables.
pub struct SymbolStore {
    connection: Connection,
}

impl SymbolStore {
    /// Open (creating/migrating if needed) the `index.db` at `path`.
    pub fn open(path: &Path) -> Result<Self, SymbolError> {
        let opened = schema::index::open(path)?;
        Ok(Self::from_connection(opened.connection))
    }

    /// Wrap an already-opened `index.db` connection.
    #[must_use]
    pub fn from_connection(connection: Connection) -> Self {
        Self { connection }
    }

    /// This store's connection, so a caller that must write other
    /// `index.db` tables in the same transaction can do so.
    #[must_use]
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// The row id of `profile`, inserting it only if this `index.db` has
    /// not seen that exact profile before.
    pub fn ensure_profile(&self, profile: &AnalysisProfile) -> Result<i64, SymbolError> {
        ensure_profile(&self.connection, profile)
    }

    /// The row id of an already-stored profile key.
    pub fn profile_id(&self, profile_key: &str) -> Result<Option<i64>, SymbolError> {
        Ok(self
            .connection
            .query_row(
                "SELECT id FROM analysis_profile WHERE profile_key = ?1",
                params![profile_key],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// How many profiles this `index.db` holds.
    pub fn profile_count(&self) -> Result<i64, SymbolError> {
        Ok(self
            .connection
            .query_row("SELECT COUNT(*) FROM analysis_profile", [], |row| {
                row.get(0)
            })?)
    }

    /// One Resource's Symbols, in source order.
    pub fn list_for_resource(&self, resource_id: ResourceId) -> Result<Vec<Symbol>, SymbolError> {
        list_for_resource(&self.connection, resource_id)
    }

    /// Replace one Resource's entire Symbol set, in its own transaction.
    ///
    /// `basis_revision` is the `resource_revision` the extraction read.
    pub fn replace_for_resource(
        &self,
        resource_id: ResourceId,
        basis_revision: &str,
        symbols: &[Symbol],
    ) -> Result<(), SymbolError> {
        let transaction = self.connection.unchecked_transaction()?;
        self.replace_in_transaction(resource_id, basis_revision, symbols)?;
        transaction.commit()?;
        Ok(())
    }

    /// Replace one Resource's whole structural result -- Symbols and the
    /// Occurrence evidence about them -- in a single transaction.
    ///
    /// This is the accepted-extraction path: `Symbol replace → local symbol
    /// row mapping → Occurrence replace → commit`. A failure anywhere rolls
    /// the whole thing back, so the previously published Symbols *and*
    /// Occurrences both survive intact; there is no state in which one was
    /// replaced and the other was not.
    pub fn replace_structure(
        &self,
        resource_id: ResourceId,
        basis_revision: &str,
        generation_id: i64,
        symbols: &[Symbol],
        occurrences: &[Occurrence],
    ) -> Result<(), SymbolError> {
        let transaction = self.connection.unchecked_transaction()?;
        let symbol_rows = self.replace_in_transaction(resource_id, basis_revision, symbols)?;
        self.replace_occurrences_in_transaction(
            resource_id,
            basis_revision,
            generation_id,
            &symbol_rows,
            occurrences,
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Replace one Resource's Occurrence evidence, in the caller's
    /// transaction, against the local symbol rows
    /// [`Self::replace_in_transaction`] just wrote.
    ///
    /// An Occurrence has no stable identity of its own: it is evidence
    /// about a span of the current source, so a re-extraction replaces the
    /// whole set rather than reconciling it. What is checked before any of
    /// it is written is that the evidence describes something current --
    /// the Resource is still ACTIVE at `basis_revision`, and
    /// `generation_id` is the generation that is *currently* STABLE.
    /// Evidence is never attached to a stale revision or to a generation
    /// that is building, aborted, or superseded.
    pub fn replace_occurrences_in_transaction(
        &self,
        resource_id: ResourceId,
        basis_revision: &str,
        generation_id: i64,
        symbol_rows: &HashMap<SymbolId, i64>,
        occurrences: &[Occurrence],
    ) -> Result<(), SymbolError> {
        replace_occurrences_in_transaction(
            &self.connection,
            resource_id,
            basis_revision,
            generation_id,
            symbol_rows,
            occurrences,
        )
    }

    /// One Resource's Occurrence evidence, in source order.
    pub fn list_occurrences_for_resource(
        &self,
        resource_id: ResourceId,
    ) -> Result<Vec<Occurrence>, SymbolError> {
        list_occurrences_for_resource(&self.connection, resource_id)
    }

    /// [`Self::replace_for_resource`]'s writes without the transaction, for
    /// a caller that already owns one on this connection -- so
    /// [`Self::replace_occurrences_in_transaction`]'s rows can land in the
    /// same commit as these Symbols.
    ///
    /// Either every row of the new set is written or none is: the old set
    /// is deleted and the new one inserted inside the caller's transaction,
    /// and there is deliberately no per-row update path.
    ///
    /// The Resource's Occurrences are deleted first. They are evidence
    /// *about* these Symbols, so they cannot outlive the set they point
    /// at -- and the foreign key would refuse the symbol delete anyway.
    /// Returns each written Symbol's local `symbol.id`, which is what an
    /// Occurrence's `containing_symbol_id` needs.
    pub fn replace_in_transaction(
        &self,
        resource_id: ResourceId,
        basis_revision: &str,
        symbols: &[Symbol],
    ) -> Result<HashMap<SymbolId, i64>, SymbolError> {
        replace_in_transaction(&self.connection, resource_id, basis_revision, symbols)
    }
}

// The `&Connection` functions below are what [`SymbolStore`]'s methods are
// built from, following the same pattern as `generation`, `watch`, and
// `component`: a caller that must commit Resource rows, journal rows, and
// a generation in the *same* transaction as these Symbols (#16 task 13's
// targeted refresh) owns that connection, so it needs this logic as
// functions rather than as methods on a store that owns one.

pub(crate) fn ensure_profile(
    connection: &Connection,
    profile: &AnalysisProfile,
) -> Result<i64, SymbolError> {
    if let Some(id) = profile_id(connection, &profile.profile_key)? {
        return Ok(id);
    }
    connection.execute(
        "INSERT INTO analysis_profile \
         (profile_key, language, analysis_mode, structural_backend, \
          structural_backend_version, semantic_backend, semantic_backend_version, \
          extractor_semantics_version, adapter_semantics_version, \
          backend_compatibility_class, capability_fingerprint, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            profile.profile_key,
            profile.language.to_string(),
            profile.analysis_mode,
            profile.structural_backend,
            profile.structural_backend_version,
            profile.semantic_backend,
            profile.semantic_backend_version,
            profile.extractor_semantics_version,
            profile.adapter_semantics_version,
            profile.backend_compatibility_class,
            profile.capability_fingerprint,
            db::now_millis_text(),
        ],
    )?;
    Ok(connection.last_insert_rowid())
}

pub(crate) fn profile_id(
    connection: &Connection,
    profile_key: &str,
) -> Result<Option<i64>, SymbolError> {
    Ok(connection
        .query_row(
            "SELECT id FROM analysis_profile WHERE profile_key = ?1",
            params![profile_key],
            |row| row.get(0),
        )
        .optional()?)
}

pub(crate) fn list_for_resource(
    connection: &Connection,
    resource_id: ResourceId,
) -> Result<Vec<Symbol>, SymbolError> {
    let mut statement = connection.prepare(&format!(
        "SELECT {SYMBOL_COLUMNS} {SYMBOL_FROM} \
         WHERE r.uid = ?1 \
         ORDER BY s.start_byte, s.end_byte DESC, s.name",
    ))?;
    let rows = statement.query_map(params![resource_id.to_bytes().to_vec()], raw_symbol_row)?;
    rows.map(|raw| decode_symbol(raw?)).collect()
}

pub(crate) fn replace_in_transaction(
    connection: &Connection,
    resource_id: ResourceId,
    basis_revision: &str,
    symbols: &[Symbol],
) -> Result<HashMap<SymbolId, i64>, SymbolError> {
    // Re-checked here, immediately before writing, rather than by the
    // caller earlier: the point is that nothing moved in between.
    let (local_resource_id, ..) = publishable_resource(connection, resource_id, basis_revision)?;

    clear_occurrence_dependents(connection, local_resource_id)?;
    connection.execute(
        "DELETE FROM occurrence WHERE resource_id = ?1",
        params![local_resource_id],
    )?;

    // A Symbol whose identity survived the edit keeps its *row*, not
    // just its `SymbolId` (#17 task 13): the graph points at these rows,
    // and deleting and re-inserting a declaration that never went away
    // would take every relation into it with them.
    let existing = local_symbol_rows(connection, local_resource_id)?;

    // Parents are written before their children, so a child's
    // parent_symbol_id always resolves. The extractor emits in tree
    // order, which already satisfies that.
    let mut local_ids: HashMap<SymbolId, i64> = HashMap::new();
    for symbol in symbols {
        let parent_local = match symbol.parent_id {
            None => None,
            Some(parent) => Some(*local_ids.get(&parent).ok_or(SymbolError::UnknownParent {
                symbol: symbol.id,
                parent,
            })?),
        };
        let values = params![
            symbol.id.to_bytes().to_vec(),
            local_resource_id,
            parent_local,
            symbol.kind.as_str(),
            symbol.name,
            symbol.qualified_name,
            symbol.signature,
            symbol.visibility.as_str(),
            i64::from(symbol.exported),
            i64::try_from(symbol.span.start_byte).unwrap_or(i64::MAX),
            i64::try_from(symbol.span.end_byte).unwrap_or(i64::MAX),
            i64::try_from(symbol.span.start.line).unwrap_or(i64::MAX),
            i64::try_from(symbol.span.start.column).unwrap_or(i64::MAX),
            i64::try_from(symbol.span.end.line).unwrap_or(i64::MAX),
            i64::try_from(symbol.span.end.column).unwrap_or(i64::MAX),
            symbol.resource_revision,
            symbol.analysis_profile_id,
        ];
        match existing.iter().find(|(id, _)| *id == symbol.id) {
            Some((_, local)) => {
                connection.execute(
                    "UPDATE symbol SET uid = ?1, resource_id = ?2, parent_symbol_id = ?3, \
                            kind = ?4, name = ?5, qualified_name = ?6, signature = ?7, \
                            visibility = ?8, exported = ?9, start_byte = ?10, end_byte = ?11, \
                            start_line = ?12, start_col = ?13, end_line = ?14, end_col = ?15, \
                            resource_revision = ?16, analysis_profile_id = ?17 \
                     WHERE id = ?18",
                    rusqlite::params_from_iter(
                        values
                            .iter()
                            .map(|value| *value as &dyn rusqlite::ToSql)
                            .chain(std::iter::once(local as &dyn rusqlite::ToSql)),
                    ),
                )?;
                local_ids.insert(symbol.id, *local);
            }
            None => {
                connection.execute(
                    "INSERT INTO symbol \
                     (uid, resource_id, parent_symbol_id, kind, name, qualified_name, signature, \
                      visibility, exported, start_byte, end_byte, start_line, start_col, \
                      end_line, end_col, resource_revision, analysis_profile_id) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, \
                             ?16, ?17)",
                    values,
                )?;
                local_ids.insert(symbol.id, connection.last_insert_rowid());
            }
        }
    }

    // Only now the declarations that are actually gone. Written after
    // the upserts, so an inconsistent incoming set is refused before
    // anything is deleted -- and children first, because a parent still
    // has rows pointing at it.
    let incoming: Vec<SymbolId> = symbols.iter().map(|symbol| symbol.id).collect();
    let mut vanished: Vec<i64> = existing
        .iter()
        .filter(|(id, _)| !incoming.contains(id))
        .map(|(_, local)| *local)
        .collect();
    vanished.sort_unstable_by(|left, right| right.cmp(left));
    for local in vanished {
        connection.execute("DELETE FROM symbol WHERE id = ?1", params![local])?;
    }

    Ok(local_ids)
}

/// Remove what is anchored to a Resource's Occurrences before those
/// Occurrences are deleted.
///
/// The unresolved references of #17 task 7 point at Occurrence rows, so
/// replacing a Resource's structure has to take them with it. The
/// relation layer's own bookkeeping -- which canonical edges lose their
/// last evidence -- belongs to `graph_lifecycle`, which runs before this
/// (#17 task 13); this is only the foreign key that this delete owns.
fn clear_occurrence_dependents(
    connection: &Connection,
    local_resource_id: i64,
) -> Result<(), SymbolError> {
    connection.execute(
        "DELETE FROM relation_candidate WHERE unresolved_reference_id IN \
         (SELECT unresolved_reference.id FROM unresolved_reference \
          JOIN occurrence ON occurrence.id = unresolved_reference.occurrence_id \
          WHERE occurrence.resource_id = ?1)",
        params![local_resource_id],
    )?;
    connection.execute(
        "DELETE FROM unresolved_reference WHERE occurrence_id IN \
         (SELECT id FROM occurrence WHERE resource_id = ?1)",
        params![local_resource_id],
    )?;
    Ok(())
}

/// This Resource's stored Symbols, as `(stable id, row id)`.
pub(crate) fn local_symbol_rows(
    connection: &Connection,
    local_resource_id: i64,
) -> Result<Vec<(SymbolId, i64)>, SymbolError> {
    let mut statement =
        connection.prepare("SELECT uid, id FROM symbol WHERE resource_id = ?1 ORDER BY id")?;
    let rows = statement.query_map(params![local_resource_id], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut found = Vec::new();
    for row in rows {
        let (uid, local) = row?;
        found.push((
            SymbolId::from_bytes(stable_bytes(&uid, "symbol.uid")),
            local,
        ));
    }
    Ok(found)
}

pub(crate) fn replace_occurrences_in_transaction(
    connection: &Connection,
    resource_id: ResourceId,
    basis_revision: &str,
    generation_id: i64,
    symbol_rows: &HashMap<SymbolId, i64>,
    occurrences: &[Occurrence],
) -> Result<(), SymbolError> {
    let (local_resource_id, ..) = publishable_resource(connection, resource_id, basis_revision)?;
    let stable = generation::current_stable(connection)?.map(|generation| generation.id);
    if stable != Some(generation_id) {
        return Err(SymbolError::GenerationNotStable {
            generation_id,
            stable,
        });
    }

    insert_occurrences(connection, local_resource_id, symbol_rows, occurrences)
}

/// Replace one Resource's whole structural result inside a *publication*
/// transaction (#16 task 13).
///
/// The difference from [`SymbolStore::replace_structure`] is which
/// generation the evidence may name. The standalone path insists on the
/// generation that is currently STABLE, because outside a publication
/// there is no other generation an Occurrence could honestly belong to.
/// Inside one, the generation that will own this evidence is the one this
/// transaction is about to publish -- BUILDING right now, STABLE before
/// the commit.
///
/// [`PublicationGrant`] is what makes that a distinction rather than a
/// loophole: it can only come from [`generation::grant_publication`] in the
/// caller's open transaction, which re-checks that the generation is
/// BUILDING and that its basis is the current Workspace revision.
/// `publication_revision` is checked against that basis here, so evidence
/// cannot be attached to a generation begun for some other revision. The
/// caller must still finish the publication (STABLE plus the stable
/// pointer swap) before committing. There is no general API for writing
/// evidence against an arbitrary BUILDING or ABORTED generation, and this
/// is not one.
pub(crate) fn replace_structure_in_publication(
    connection: &Connection,
    grant: &PublicationGrant,
    publication_revision: &str,
    resource_id: ResourceId,
    basis_revision: &str,
    symbols: &[Symbol],
    occurrences: &[Occurrence],
) -> Result<(), SymbolError> {
    if grant.basis_workspace_revision() != publication_revision {
        return Err(SymbolError::PublicationBasisMismatch {
            generation_id: grant.generation_id(),
            basis: grant.basis_workspace_revision().to_owned(),
            publication: publication_revision.to_owned(),
        });
    }
    let symbol_rows = replace_in_transaction(connection, resource_id, basis_revision, symbols)?;
    let (local_resource_id, ..) = publishable_resource(connection, resource_id, basis_revision)?;
    insert_occurrences(connection, local_resource_id, &symbol_rows, occurrences)
}

/// The Occurrence write itself, shared by both paths above so that which
/// generation may be named is the only thing that differs between them.
fn insert_occurrences(
    connection: &Connection,
    local_resource_id: i64,
    symbol_rows: &HashMap<SymbolId, i64>,
    occurrences: &[Occurrence],
) -> Result<(), SymbolError> {
    clear_occurrence_dependents(connection, local_resource_id)?;
    connection.execute(
        "DELETE FROM occurrence WHERE resource_id = ?1",
        params![local_resource_id],
    )?;

    for occurrence in occurrences {
        let containing = match occurrence.containing_symbol_id {
            None => None,
            Some(symbol) => Some(
                *symbol_rows
                    .get(&symbol)
                    .ok_or(SymbolError::UnknownContainingSymbol { symbol })?,
            ),
        };
        connection.execute(
            "INSERT INTO occurrence \
             (resource_id, containing_symbol_id, kind, start_byte, end_byte, start_line, \
              start_col, end_line, end_col, relation_id, analysis_profile_id, \
              resolution_context_id, resource_revision, generation) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, NULL, ?11, ?12)",
            params![
                local_resource_id,
                containing,
                occurrence.kind.as_str(),
                i64::try_from(occurrence.span.start_byte).unwrap_or(i64::MAX),
                i64::try_from(occurrence.span.end_byte).unwrap_or(i64::MAX),
                i64::try_from(occurrence.span.start.line).unwrap_or(i64::MAX),
                i64::try_from(occurrence.span.start.column).unwrap_or(i64::MAX),
                i64::try_from(occurrence.span.end.line).unwrap_or(i64::MAX),
                i64::try_from(occurrence.span.end.column).unwrap_or(i64::MAX),
                occurrence.analysis_profile_id,
                occurrence.resource_revision,
                occurrence.generation_id,
            ],
        )?;
    }

    Ok(())
}

pub(crate) fn list_occurrences_for_resource(
    connection: &Connection,
    resource_id: ResourceId,
) -> Result<Vec<Occurrence>, SymbolError> {
    let mut statement = connection.prepare(
        "SELECT o.kind, o.start_byte, o.end_byte, o.start_line, o.start_col, o.end_line, \
                o.end_col, cs.uid, o.relation_id, o.analysis_profile_id, \
                o.resolution_context_id, o.resource_revision, o.generation \
         FROM occurrence o \
         JOIN resource r ON r.id = o.resource_id \
         LEFT JOIN symbol cs ON cs.id = o.containing_symbol_id \
         WHERE r.uid = ?1 \
         ORDER BY o.start_byte, o.end_byte",
    )?;
    let rows = statement.query_map(params![resource_id.to_bytes().to_vec()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, Option<Vec<u8>>>(7)?,
            row.get::<_, Option<i64>>(8)?,
            row.get::<_, i64>(9)?,
            row.get::<_, Option<i64>>(10)?,
            row.get::<_, String>(11)?,
            row.get::<_, i64>(12)?,
        ))
    })?;
    rows.map(|row| {
        let raw = row?;
        Ok(Occurrence {
            resource_id,
            containing_symbol_id: raw
                .7
                .map(|bytes| SymbolId::from_bytes(stable_bytes(&bytes, "containing symbol.uid"))),
            kind: OccurrenceKind::parse(&raw.0)?,
            span: SourceSpan {
                start_byte: usize::try_from(raw.1).unwrap_or(0),
                end_byte: usize::try_from(raw.2).unwrap_or(0),
                start: SourcePoint::new(
                    usize::try_from(raw.3).unwrap_or(0),
                    usize::try_from(raw.4).unwrap_or(0),
                ),
                end: SourcePoint::new(
                    usize::try_from(raw.5).unwrap_or(0),
                    usize::try_from(raw.6).unwrap_or(0),
                ),
            },
            relation_id: raw.8,
            analysis_profile_id: raw.9,
            resolution_context_id: raw.10,
            resource_revision: raw.11,
            generation_id: raw.12,
        })
    })
    .collect()
}

/// The local row id of an ACTIVE Resource still at `basis_revision` --
/// the precondition every structural write shares.
fn publishable_resource(
    connection: &Connection,
    resource_id: ResourceId,
    basis_revision: &str,
) -> Result<(i64, ResourceState, String), SymbolError> {
    let (local_id, state, current_revision) = resource_row(connection, resource_id)?;
    if state != ResourceState::Active {
        return Err(SymbolError::ResourceNotActive { resource_id, state });
    }
    if current_revision != basis_revision {
        return Err(SymbolError::RevisionMismatch {
            resource_id,
            basis: basis_revision.to_owned(),
            current: current_revision,
        });
    }
    Ok((local_id, state, current_revision))
}

fn resource_row(
    connection: &Connection,
    resource_id: ResourceId,
) -> Result<(i64, ResourceState, String), SymbolError> {
    let row: Option<(i64, String, String)> = connection
        .query_row(
            "SELECT id, state, resource_revision FROM resource WHERE uid = ?1",
            params![resource_id.to_bytes().to_vec()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let (local_id, state, revision) = row.ok_or(SymbolError::UnknownResource { resource_id })?;
    Ok((local_id, ResourceState::parse(&state)?, revision))
}

pub(crate) type RawSymbolRow = (
    Vec<u8>,
    Vec<u8>,
    Option<Vec<u8>>,
    String,
    String,
    String,
    Option<String>,
    String,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    String,
    i64,
);

pub(crate) fn raw_symbol_row(row: &Row<'_>) -> rusqlite::Result<RawSymbolRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
        row.get(15)?,
        row.get(16)?,
    ))
}

pub(crate) fn decode_symbol(raw: RawSymbolRow) -> Result<Symbol, SymbolError> {
    Ok(Symbol {
        id: SymbolId::from_bytes(stable_bytes(&raw.0, "symbol.uid")),
        resource_id: ResourceId::from_bytes(stable_bytes(&raw.1, "resource.uid")),
        parent_id: raw
            .2
            .map(|bytes| SymbolId::from_bytes(stable_bytes(&bytes, "parent symbol.uid"))),
        kind: SymbolKind::parse(&raw.3)?,
        name: raw.4,
        qualified_name: raw.5,
        signature: raw.6,
        visibility: Visibility::parse(&raw.7)?,
        exported: raw.8 != 0,
        span: SourceSpan {
            start_byte: usize::try_from(raw.9).unwrap_or(0),
            end_byte: usize::try_from(raw.10).unwrap_or(0),
            start: SourcePoint::new(
                usize::try_from(raw.11).unwrap_or(0),
                usize::try_from(raw.12).unwrap_or(0),
            ),
            end: SourcePoint::new(
                usize::try_from(raw.13).unwrap_or(0),
                usize::try_from(raw.14).unwrap_or(0),
            ),
        },
        resource_revision: raw.15,
        analysis_profile_id: raw.16,
    })
}

fn stable_bytes(bytes: &[u8], column: &str) -> [u8; 16] {
    bytes
        .try_into()
        .unwrap_or_else(|_| panic!("{column} must hold a 16-byte value"))
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{
        config::WorkspaceConfig,
        extract::{Extraction, ExtractionStatus, assign_ids, extract, resolve_occurrences},
        parser::{ParserDialect, ParserRegistry, SourceBasis, dialect_for_resource},
        resource::Resource,
        scan::BaselineScan,
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    const SOURCE: &str = "\
export class Thing {
  value = 1

  measure(): number {
    return this.value
  }
}

export function top(): number { return 1 }
";

    /// A Workspace with one real Resource, published by the task 4
    /// baseline so every Symbol row hangs off a genuine `resource` row.
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-symbol-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            fs::create_dir_all(&root).expect("workspace root");
            Self { base, root }
        }

        fn db_path(&self) -> PathBuf {
            self.base.join("data").join("index.db")
        }

        fn write(&self, rel: &str, contents: &str) {
            fs::write(self.root.join(rel), contents).expect("fixture file");
        }

        /// Publish the Resource baseline and return the store plus the
        /// Resource for `rel`.
        fn baseline(&self, rel: &str) -> (SymbolStore, Resource) {
            let (store, resource, _) = self.baseline_with_generation(rel);
            (store, resource)
        }

        /// The same, plus the stable generation Occurrence evidence must
        /// be published against.
        fn baseline_with_generation(&self, rel: &str) -> (SymbolStore, Resource, i64) {
            let engine = BaselineScan::open(&self.db_path()).expect("index.db");
            let report = engine
                .run_initial_scan(&self.root, &WorkspaceConfig::default(), "workspace-rev-1")
                .expect("baseline scan");
            let resource = engine
                .resources()
                .get_active_by_path_key(rel)
                .expect("lookup")
                .expect("the fixture file is a Resource");
            drop(engine);
            (
                SymbolStore::open(&self.db_path()).expect("index.db"),
                resource,
                report.generation.id,
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn extraction_of(dialect: ParserDialect, source: &str) -> Extraction {
        let mut registry = ParserRegistry::new();
        let tree = registry
            .parse(dialect, source.as_bytes(), SourceBasis::default())
            .expect("parse");
        extract(&tree, source.as_bytes())
    }

    /// The whole pipeline for one file: parse, extract, identify, store.
    fn publish(
        store: &SymbolStore,
        resource: &Resource,
        source: &str,
    ) -> Result<Vec<Symbol>, SymbolError> {
        let dialect = dialect_for_resource(resource).expect("a supported dialect");
        let extraction = extraction_of(dialect, source);
        assert!(extraction.is_accepted());
        let profile_id = store.ensure_profile(&extraction.profile)?;
        let previous = store.list_for_resource(resource.id)?;
        let symbols = assign_ids(&previous, &extraction, resource, profile_id);
        store.replace_for_resource(resource.id, &resource.resource_revision, &symbols)?;
        Ok(symbols)
    }

    /// The accepted-extraction path: Symbols and their evidence, together.
    fn publish_structure(
        store: &SymbolStore,
        resource: &Resource,
        generation_id: i64,
        source: &str,
    ) -> Result<(Vec<Symbol>, Vec<Occurrence>), SymbolError> {
        let dialect = dialect_for_resource(resource).expect("a supported dialect");
        let extraction = extraction_of(dialect, source);
        assert!(extraction.is_accepted());
        let profile_id = store.ensure_profile(&extraction.profile)?;
        let previous = store.list_for_resource(resource.id)?;
        let symbols = assign_ids(&previous, &extraction, resource, profile_id);
        let occurrences =
            resolve_occurrences(&extraction, &symbols, resource, profile_id, generation_id);
        store.replace_structure(
            resource.id,
            &resource.resource_revision,
            generation_id,
            &symbols,
            &occurrences,
        )?;
        Ok((symbols, occurrences))
    }

    #[test]
    fn a_resources_symbols_round_trip_with_their_hierarchy_and_spans() {
        let fixture = Fixture::create("round-trip");
        fixture.write("thing.ts", SOURCE);
        let (store, resource) = fixture.baseline("thing.ts");

        let written = publish(&store, &resource, SOURCE).expect("publish");
        let stored = store.list_for_resource(resource.id).expect("list");

        assert_eq!(stored, written, "what was written is what reads back");
        let class = &stored[0];
        let value = &stored[1];
        assert_eq!(class.qualified_name, "Thing");
        assert_eq!(value.qualified_name, "Thing.value");
        assert_eq!(value.parent_id, Some(class.id), "the parent link survives");
        assert_eq!(class.resource_revision, resource.resource_revision);
        let declaration = &SOURCE[class.span.start_byte..class.span.end_byte];
        assert!(
            declaration.starts_with("class Thing {") && declaration.ends_with('}'),
            "the stored span still reads the declaration: {declaration:?}"
        );
        // `export` sits outside the declaration, so the span starts at the
        // keyword and `exported` carries the rest.
        assert_eq!(class.span.start, SourcePoint::new(0, 7));
        assert!(class.exported);
    }

    #[test]
    fn one_profile_is_created_and_then_reused() {
        let fixture = Fixture::create("profile-reuse");
        fixture.write("thing.ts", SOURCE);
        let (store, resource) = fixture.baseline("thing.ts");

        publish(&store, &resource, SOURCE).expect("publish");
        let after_first = store.profile_count().expect("count");
        publish(&store, &resource, SOURCE).expect("publish again");
        let edited = SOURCE.replace("return this.value", "return this.value + 1");
        publish(&store, &resource, &edited).expect("publish edited");

        assert_eq!(after_first, 1);
        assert_eq!(
            store.profile_count().expect("count"),
            1,
            "the same descriptor must not keep creating profiles"
        );
    }

    #[test]
    fn a_dialect_is_never_lost_from_a_profile_identity() {
        let profiles: Vec<AnalysisProfile> = [
            ParserDialect::TypeScript,
            ParserDialect::Tsx,
            ParserDialect::JavaScript,
            ParserDialect::Jsx,
            ParserDialect::Python,
            ParserDialect::Rust,
        ]
        .into_iter()
        .map(|dialect| {
            let mut registry = ParserRegistry::new();
            AnalysisProfile::of(&registry.describe(dialect).expect("grammar"))
        })
        .collect();

        let mut keys: Vec<&str> = profiles
            .iter()
            .map(|profile| profile.profile_key.as_str())
            .collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(
            keys.len(),
            profiles.len(),
            "TS/TSX and JS/JSX must not collapse into one profile"
        );
        for profile in &profiles {
            assert_eq!(profile.analysis_mode, ANALYSIS_MODE_STRUCTURAL);
            assert_eq!(
                profile.extractor_semantics_version,
                EXTRACTOR_SEMANTICS_VERSION
            );
        }

        // JS and JSX share a grammar, so the dialect is what keeps them
        // apart -- and the compatibility class is where it is recorded.
        assert_ne!(
            profiles[2].backend_compatibility_class,
            profiles[3].backend_compatibility_class
        );
    }

    #[test]
    fn no_semantic_backend_is_claimed_yet() {
        let fixture = Fixture::create("no-semantic");
        fixture.write("thing.ts", SOURCE);
        let (store, resource) = fixture.baseline("thing.ts");
        publish(&store, &resource, SOURCE).expect("publish");

        let (semantic, semantic_version): (Option<String>, Option<String>) = store
            .connection()
            .query_row(
                "SELECT semantic_backend, semantic_backend_version FROM analysis_profile",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("the profile row");
        assert_eq!(semantic, None);
        assert_eq!(semantic_version, None);
    }

    #[test]
    fn a_revision_mismatch_refuses_the_replacement_and_keeps_the_existing_set() {
        let fixture = Fixture::create("revision-mismatch");
        fixture.write("thing.ts", SOURCE);
        let (store, resource) = fixture.baseline("thing.ts");
        let published = publish(&store, &resource, SOURCE).expect("publish");

        // The extraction was based on a revision the Resource no longer has.
        let error = store
            .replace_for_resource(resource.id, "99", &published[..1])
            .expect_err("a stale extraction must not be written");

        assert!(matches!(error, SymbolError::RevisionMismatch { .. }));
        assert!(error.is_retryable());
        assert_eq!(
            store.list_for_resource(resource.id).expect("list"),
            published,
            "the existing Symbol set is left exactly as it was"
        );
    }

    #[test]
    fn a_deleted_resource_refuses_the_replacement() {
        let fixture = Fixture::create("deleted-resource");
        fixture.write("thing.ts", SOURCE);
        let (store, resource) = fixture.baseline("thing.ts");
        let published = publish(&store, &resource, SOURCE).expect("publish");

        store
            .connection()
            .execute(
                "UPDATE resource SET state = 'DELETED', path_key = '/tombstone/x' WHERE uid = ?1",
                params![resource.id.to_bytes().to_vec()],
            )
            .expect("tombstone the resource");

        let error = store
            .replace_for_resource(resource.id, &resource.resource_revision, &published)
            .expect_err("a tombstoned Resource takes no new symbols");
        assert!(matches!(error, SymbolError::ResourceNotActive { .. }));
    }

    #[test]
    fn a_failed_replacement_leaves_the_previous_set_whole() {
        let fixture = Fixture::create("failed-apply");
        fixture.write("thing.ts", SOURCE);
        let (store, resource) = fixture.baseline("thing.ts");
        let published = publish(&store, &resource, SOURCE).expect("publish");

        // A child whose parent is not part of the replacement: the delete
        // has already run inside the transaction when this is discovered,
        // so only a rollback can save the previous set.
        let mut broken = published.clone();
        broken.remove(0);
        let error = store
            .replace_for_resource(resource.id, &resource.resource_revision, &broken)
            .expect_err("an inconsistent set must not be written");

        assert!(matches!(error, SymbolError::UnknownParent { .. }));
        assert_eq!(
            store.list_for_resource(resource.id).expect("list"),
            published,
            "a half-applied replacement must not survive"
        );
    }

    #[test]
    fn a_replacement_is_whole_set_not_row_by_row() {
        let fixture = Fixture::create("whole-set");
        fixture.write("thing.ts", SOURCE);
        let (store, resource) = fixture.baseline("thing.ts");
        let first = publish(&store, &resource, SOURCE).expect("publish");

        let trimmed = "export class Thing {\n  value = 1\n}\n";
        let second = publish(&store, &resource, trimmed).expect("publish trimmed");
        let stored = store.list_for_resource(resource.id).expect("list");

        assert_eq!(stored, second);
        assert!(
            !stored
                .iter()
                .any(|symbol| symbol.qualified_name == "Thing.measure"),
            "declarations the file no longer has are gone, not left behind"
        );
        assert_eq!(
            first
                .iter()
                .find(|symbol| symbol.qualified_name == "Thing")
                .map(|symbol| symbol.id),
            stored
                .iter()
                .find(|symbol| symbol.qualified_name == "Thing")
                .map(|symbol| symbol.id),
            "and what survived kept its identity"
        );
    }

    #[test]
    fn symbols_and_other_rows_can_share_one_transaction() {
        let fixture = Fixture::create("shared-transaction");
        fixture.write("thing.ts", SOURCE);
        let (store, resource) = fixture.baseline("thing.ts");
        let dialect = dialect_for_resource(&resource).expect("dialect");
        let extraction = extraction_of(dialect, SOURCE);
        let profile_id = store.ensure_profile(&extraction.profile).expect("profile");
        let symbols = assign_ids(&[], &extraction, &resource, profile_id);

        // The baseline scan already published this file's structure
        // (#16 task 14), so what a rollback must restore is that set.
        let baseline = store.list_for_resource(resource.id).expect("list");
        assert!(!baseline.is_empty());

        // The caller owns the transaction, which is what lets #16 task 9
        // commit Occurrence rows alongside these Symbols. Dropping it must
        // take the replacement with it.
        let transaction = store
            .connection()
            .unchecked_transaction()
            .expect("transaction");
        store
            .replace_in_transaction(resource.id, &resource.resource_revision, &symbols)
            .expect("replace");
        drop(transaction);

        assert_eq!(
            store.list_for_resource(resource.id).expect("list"),
            baseline,
            "the replacement belonged to the caller's transaction"
        );
    }

    #[test]
    fn a_symbol_only_replacement_creates_no_evidence_or_relation_rows() {
        let fixture = Fixture::create("no-occurrence");
        fixture.write("thing.ts", SOURCE);
        let (store, resource) = fixture.baseline("thing.ts");
        publish(&store, &resource, SOURCE).expect("publish");

        for table in [
            "occurrence",
            "relation",
            "graph_entity",
            "unresolved_reference",
        ] {
            let count: i64 = store
                .connection()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .expect("count");
            assert_eq!(count, 0, "{table} is not this task's business");
        }
        assert!(
            !store
                .list_for_resource(resource.id)
                .expect("list")
                .is_empty()
        );
    }

    #[test]
    fn no_source_body_reaches_the_symbol_table() {
        let fixture = Fixture::create("no-body");
        let source = "export function top(): number {\n  const secretMarker = 41\n  return secretMarker + 1\n}\n";
        fixture.write("thing.ts", source);
        let (store, resource) = fixture.baseline("thing.ts");
        publish(&store, &resource, source).expect("publish");
        drop(store);

        let bytes = fs::read(fixture.db_path()).expect("index.db bytes");
        for needle in ["secretMarker", "return"] {
            assert!(
                !bytes
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes()),
                "index.db must not contain {needle:?}"
            );
        }
    }

    #[test]
    fn occurrences_round_trip_with_their_containment_and_spans() {
        let fixture = Fixture::create("occurrence-round-trip");
        fixture.write("thing.ts", SOURCE);
        let (store, resource, generation) = fixture.baseline_with_generation("thing.ts");

        let (symbols, written) =
            publish_structure(&store, &resource, generation, SOURCE).expect("publish");
        let stored = store
            .list_occurrences_for_resource(resource.id)
            .expect("list");

        assert_eq!(stored, written, "what was written is what reads back");
        assert!(!stored.is_empty());
        let measure = symbols
            .iter()
            .find(|symbol| symbol.qualified_name == "Thing.measure")
            .expect("measure");
        let definition = stored
            .iter()
            .find(|occurrence| {
                occurrence.kind == OccurrenceKind::Definition
                    && occurrence.containing_symbol_id == Some(measure.id)
            })
            .expect("a definition for measure");
        assert_eq!(
            &SOURCE[definition.span.start_byte..definition.span.end_byte],
            "measure",
            "the stored span is the name token"
        );
        assert_eq!(definition.resource_revision, resource.resource_revision);
        assert_eq!(definition.generation_id, generation);
    }

    #[test]
    fn a_structural_occurrence_resolves_nothing() {
        let fixture = Fixture::create("no-resolution");
        fixture.write("thing.ts", SOURCE);
        let (store, resource, generation) = fixture.baseline_with_generation("thing.ts");
        publish_structure(&store, &resource, generation, SOURCE).expect("publish");

        for occurrence in store
            .list_occurrences_for_resource(resource.id)
            .expect("list")
        {
            assert_eq!(occurrence.relation_id, None);
            assert_eq!(occurrence.resolution_context_id, None);
        }
        for table in [
            "relation",
            "graph_entity",
            "unresolved_reference",
            "relation_candidate",
            "resolution_context",
        ] {
            let count: i64 = store
                .connection()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .expect("count");
            assert_eq!(count, 0, "{table} belongs to I3/I4, not to this tier");
        }
    }

    #[test]
    fn evidence_is_only_published_against_the_current_stable_generation() {
        let fixture = Fixture::create("generation-basis");
        fixture.write("thing.ts", SOURCE);
        let (store, resource, generation) = fixture.baseline_with_generation("thing.ts");
        let dialect = dialect_for_resource(&resource).expect("dialect");
        let extraction = extraction_of(dialect, SOURCE);
        let profile_id = store.ensure_profile(&extraction.profile).expect("profile");
        let symbols = assign_ids(&[], &extraction, &resource, profile_id);
        // What the baseline scan published, which a refused publication
        // must leave exactly as it is.
        let baseline_symbols = store.list_for_resource(resource.id).expect("list");
        let baseline_occurrences = store
            .list_occurrences_for_resource(resource.id)
            .expect("list");

        // A generation that is merely BUILDING is not something evidence
        // may be attached to.
        let building =
            generation::begin_generation(store.connection(), "workspace-rev-1").expect("begin");
        let stale = resolve_occurrences(&extraction, &symbols, &resource, profile_id, building.id);
        let error = store
            .replace_structure(
                resource.id,
                &resource.resource_revision,
                building.id,
                &symbols,
                &stale,
            )
            .expect_err("a non-stable generation must be refused");

        assert!(matches!(error, SymbolError::GenerationNotStable { .. }));
        assert_eq!(
            store.list_for_resource(resource.id).expect("list"),
            baseline_symbols,
            "the refused publication wrote no Symbols either"
        );
        assert_eq!(
            store
                .list_occurrences_for_resource(resource.id)
                .expect("list"),
            baseline_occurrences
        );

        // The stable one is accepted.
        publish_structure(&store, &resource, generation, SOURCE).expect("publish");
        assert!(
            !store
                .list_occurrences_for_resource(resource.id)
                .expect("list")
                .is_empty()
        );
    }

    #[test]
    fn a_revision_mismatch_preserves_both_symbols_and_occurrences() {
        let fixture = Fixture::create("occurrence-revision");
        fixture.write("thing.ts", SOURCE);
        let (store, resource, generation) = fixture.baseline_with_generation("thing.ts");
        let (symbols, occurrences) =
            publish_structure(&store, &resource, generation, SOURCE).expect("publish");

        let error = store
            .replace_structure(resource.id, "99", generation, &symbols[..1], &[])
            .expect_err("a stale extraction must not be written");

        assert!(matches!(error, SymbolError::RevisionMismatch { .. }));
        assert_eq!(store.list_for_resource(resource.id).expect("list"), symbols);
        assert_eq!(
            store
                .list_occurrences_for_resource(resource.id)
                .expect("list"),
            occurrences,
            "both sets survive together"
        );
    }

    #[test]
    fn a_failing_occurrence_apply_rolls_the_symbol_replacement_back_too() {
        let fixture = Fixture::create("occurrence-rollback");
        fixture.write("thing.ts", SOURCE);
        let (store, resource, generation) = fixture.baseline_with_generation("thing.ts");
        let (symbols, occurrences) =
            publish_structure(&store, &resource, generation, SOURCE).expect("publish");

        // Evidence naming a Symbol that is not in the replacement: the
        // Symbols have already been rewritten when this is discovered.
        let orphan = Occurrence {
            containing_symbol_id: Some(SymbolId::generate()),
            ..occurrences[0].clone()
        };
        let trimmed = &symbols[..1];
        let error = store
            .replace_structure(
                resource.id,
                &resource.resource_revision,
                generation,
                trimmed,
                &[orphan],
            )
            .expect_err("inconsistent evidence must not be written");

        assert!(matches!(error, SymbolError::UnknownContainingSymbol { .. }));
        assert_eq!(
            store.list_for_resource(resource.id).expect("list"),
            symbols,
            "the Symbol replacement rolled back with the evidence"
        );
        assert_eq!(
            store
                .list_occurrences_for_resource(resource.id)
                .expect("list"),
            occurrences
        );
    }

    #[test]
    fn an_unchanged_re_extraction_publishes_the_same_evidence() {
        let fixture = Fixture::create("occurrence-stable");
        fixture.write("thing.ts", SOURCE);
        let (store, resource, generation) = fixture.baseline_with_generation("thing.ts");

        let (first_symbols, first) =
            publish_structure(&store, &resource, generation, SOURCE).expect("publish");
        let (second_symbols, second) =
            publish_structure(&store, &resource, generation, SOURCE).expect("publish again");

        assert_eq!(first, second);
        assert_eq!(
            first_symbols.iter().map(|s| s.id).collect::<Vec<_>>(),
            second_symbols.iter().map(|s| s.id).collect::<Vec<_>>(),
            "stable Symbol identities mean stable containment"
        );
        assert_eq!(
            store
                .list_occurrences_for_resource(resource.id)
                .expect("list"),
            second
        );
    }

    #[test]
    fn a_partial_parse_leaves_the_accepted_evidence_alone() {
        let fixture = Fixture::create("occurrence-partial");
        fixture.write("thing.ts", SOURCE);
        let (store, resource, generation) = fixture.baseline_with_generation("thing.ts");
        let (symbols, occurrences) =
            publish_structure(&store, &resource, generation, SOURCE).expect("publish");

        let broken = "export class Thing {\n  measure(): number {\n    return\n}\n";
        let extraction = extraction_of(ParserDialect::TypeScript, broken);
        assert!(!extraction.is_accepted());

        // The caller never reaches a replacement for a result it may not
        // accept, so nothing is written.
        assert_eq!(store.list_for_resource(resource.id).expect("list"), symbols);
        assert_eq!(
            store
                .list_occurrences_for_resource(resource.id)
                .expect("list"),
            occurrences
        );
    }

    #[test]
    fn a_svelte_component_publishes_its_script_in_the_components_coordinates() {
        let source = "<script lang=\"ts\">\n  export function go() { run() }\n</script>\n\
                      <p>{go}</p>\n";
        let fixture = Fixture::create("occurrence-svelte");
        fixture.write("View.svelte", source);
        let (_store, resource, _) = fixture.baseline_with_generation("View.svelte");
        let dialect = dialect_for_resource(&resource).expect("dialect");
        let extraction = extraction_of(dialect, source);

        // #19 task 11: the embedded script is extracted, and every span
        // indexes the component rather than the fragment.
        assert_eq!(extraction.status, ExtractionStatus::Complete);
        assert!(extraction.symbols.iter().any(|symbol| symbol.name == "go"));
        let template = extraction
            .occurrences
            .iter()
            .filter(|occurrence| occurrence.kind == OccurrenceKind::ReferenceSite)
            .map(|occurrence| &source[occurrence.span.start_byte..occurrence.span.end_byte])
            .collect::<Vec<_>>();
        assert_eq!(template, vec!["go"], "the template use of a bound name");
        for occurrence in &extraction.occurrences {
            assert!(
                occurrence.span.end_byte <= source.len(),
                "a span outside the component would be a fragment offset"
            );
        }
    }

    #[test]
    fn no_source_text_reaches_the_occurrence_table() {
        let fixture = Fixture::create("occurrence-no-body");
        let source =
            "import { secretHelper } from './m'\nexport function top() { return secretHelper() }\n";
        fixture.write("thing.ts", source);
        let (store, resource, generation) = fixture.baseline_with_generation("thing.ts");
        publish_structure(&store, &resource, generation, source).expect("publish");

        let stored: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM occurrence", [], |row| row.get(0))
            .expect("count");
        assert!(stored > 0, "there is evidence to find");
        drop(store);

        let bytes = fs::read(fixture.db_path()).expect("index.db bytes");
        // Source *text* is what must not be there. A name is identity,
        // not a body: the Symbol table has always stored declaration
        // names, and since #17 task 7 an unresolved reference stores the
        // name it looked for and the module it looked in. What no row
        // may hold is the source around them.
        for needle in [
            "return secretHelper()",
            "export function top",
            "import { secretHelper }",
        ] {
            assert!(
                !bytes
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes()),
                "index.db must not contain {needle:?}; spans are offsets, not text"
            );
        }
    }
}
