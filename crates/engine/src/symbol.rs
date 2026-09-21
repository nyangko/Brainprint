//! Canonical Symbol model, `analysis_profile` reuse, and the
//! Resource-owned replacement primitive (#16 task 8 / #13 task 6 §7-8).
//!
//! Scope: this module owns the typed boundary over the `symbol` and
//! `analysis_profile` columns #15 task 5's schema already created. It
//! defines no new column and redesigns none. It does not parse -- turning a
//! parse tree into candidates is [`crate::extract`] -- and it knows nothing
//! about Occurrence (#16 task 9), Relation (I3), or semantic resolution
//! (I4).
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
//! [`SymbolStore::replace_in_transaction`] is the same primitive without
//! the transaction, so #16 task 9's Occurrence rows can be committed in the
//! same transaction as the Symbols they point at.

use std::{collections::HashMap, error::Error, fmt, path::Path};

use brainprint_core::{ResourceId, SymbolId};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::{
    db::{self, DbOpenError},
    parser::{ParserDescriptor, SourcePoint, SourceSpan, StructuralCapability},
    resource::{ResourceError, ResourceLanguage, ResourceState},
    schema,
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
    pub extractor_semantics_version: &'static str,
    pub adapter_semantics_version: &'static str,
    /// Which backend/dialect combination a result is comparable across.
    pub backend_compatibility_class: String,
    pub capability_fingerprint: String,
}

impl AnalysisProfile {
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
    UnknownSymbolKind {
        raw: String,
    },
    UnknownVisibility {
        raw: String,
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
            Self::UnknownSymbolKind { raw } => write!(formatter, "unknown symbol kind {raw:?}"),
            Self::UnknownVisibility { raw } => write!(formatter, "unknown visibility {raw:?}"),
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

impl From<rusqlite::Error> for SymbolError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

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
        if let Some(id) = self.profile_id(&profile.profile_key)? {
            return Ok(id);
        }
        self.connection.execute(
            "INSERT INTO analysis_profile \
             (profile_key, language, analysis_mode, structural_backend, \
              structural_backend_version, semantic_backend, semantic_backend_version, \
              extractor_semantics_version, adapter_semantics_version, \
              backend_compatibility_class, capability_fingerprint, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?7, ?8, ?9, ?10)",
            params![
                profile.profile_key,
                profile.language.to_string(),
                profile.analysis_mode,
                profile.structural_backend,
                profile.structural_backend_version,
                profile.extractor_semantics_version,
                profile.adapter_semantics_version,
                profile.backend_compatibility_class,
                profile.capability_fingerprint,
                db::now_millis_text(),
            ],
        )?;
        Ok(self.connection.last_insert_rowid())
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
        let mut statement = self.connection.prepare(
            "SELECT s.uid, r.uid, p.uid, s.kind, s.name, s.qualified_name, s.signature, \
                    s.visibility, s.exported, s.start_byte, s.end_byte, s.start_line, \
                    s.start_col, s.end_line, s.end_col, s.resource_revision, \
                    s.analysis_profile_id \
             FROM symbol s \
             JOIN resource r ON r.id = s.resource_id \
             LEFT JOIN symbol p ON p.id = s.parent_symbol_id \
             WHERE r.uid = ?1 \
             ORDER BY s.start_byte, s.end_byte DESC, s.name",
        )?;
        let rows = statement.query_map(params![resource_id.to_bytes().to_vec()], raw_symbol_row)?;
        rows.map(|raw| decode_symbol(raw?)).collect()
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

    /// [`Self::replace_for_resource`]'s writes without the transaction, for
    /// a caller that already owns one on this connection -- so #16 task 9's
    /// Occurrence rows can land in the same commit as these Symbols.
    ///
    /// Either every row of the new set is written or none is: the old set
    /// is deleted and the new one inserted inside the caller's transaction,
    /// and there is deliberately no per-row update path.
    pub fn replace_in_transaction(
        &self,
        resource_id: ResourceId,
        basis_revision: &str,
        symbols: &[Symbol],
    ) -> Result<(), SymbolError> {
        // Re-checked here, immediately before writing, rather than by the
        // caller earlier: the point is that nothing moved in between.
        let (local_resource_id, state, current_revision) = self.resource_row(resource_id)?;
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

        self.connection.execute(
            "DELETE FROM symbol WHERE resource_id = ?1",
            params![local_resource_id],
        )?;

        // Parents are inserted before their children, so a child's
        // parent_symbol_id always resolves. The extractor emits in tree
        // order, which already satisfies that.
        let mut local_ids: HashMap<SymbolId, i64> = HashMap::new();
        for symbol in symbols {
            let parent_local = match symbol.parent_id {
                None => None,
                Some(parent) => {
                    Some(*local_ids.get(&parent).ok_or(SymbolError::UnknownParent {
                        symbol: symbol.id,
                        parent,
                    })?)
                }
            };
            self.connection.execute(
                "INSERT INTO symbol \
                 (uid, resource_id, parent_symbol_id, kind, name, qualified_name, signature, \
                  visibility, exported, start_byte, end_byte, start_line, start_col, end_line, \
                  end_col, resource_revision, analysis_profile_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, \
                         ?16, ?17)",
                params![
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
                ],
            )?;
            local_ids.insert(symbol.id, self.connection.last_insert_rowid());
        }

        Ok(())
    }

    fn resource_row(
        &self,
        resource_id: ResourceId,
    ) -> Result<(i64, ResourceState, String), SymbolError> {
        let row: Option<(i64, String, String)> = self
            .connection
            .query_row(
                "SELECT id, state, resource_revision FROM resource WHERE uid = ?1",
                params![resource_id.to_bytes().to_vec()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (local_id, state, revision) =
            row.ok_or(SymbolError::UnknownResource { resource_id })?;
        Ok((local_id, ResourceState::parse(&state)?, revision))
    }
}

type RawSymbolRow = (
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

fn raw_symbol_row(row: &Row<'_>) -> rusqlite::Result<RawSymbolRow> {
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

fn decode_symbol(raw: RawSymbolRow) -> Result<Symbol, SymbolError> {
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
        extract::{Extraction, assign_ids, extract},
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
            let engine = BaselineScan::open(&self.db_path()).expect("index.db");
            engine
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

        // The caller owns the transaction, which is what lets #16 task 9
        // commit Occurrence rows alongside these Symbols. Dropping it must
        // take the Symbols with it.
        let transaction = store
            .connection()
            .unchecked_transaction()
            .expect("transaction");
        store
            .replace_in_transaction(resource.id, &resource.resource_revision, &symbols)
            .expect("replace");
        drop(transaction);

        assert!(
            store
                .list_for_resource(resource.id)
                .expect("list")
                .is_empty(),
            "the replacement belonged to the caller's transaction"
        );
    }

    #[test]
    fn extraction_creates_no_occurrence_or_relation_rows() {
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
}
