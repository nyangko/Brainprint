//! Structured files/tree/list and Resource/Symbol locate-search over the
//! current index (#16 task 10 / #5 search-and-locate contract).
//!
//! Everything here answers from `index.db` alone. Nothing in this module
//! walks the filesystem, opens a source file, or parses anything -- that is
//! the point: the repeated `ls` / `find` / `rg <symbol>` loop is replaced
//! by the Resource inventory and the structural Symbol index that tasks
//! 1-9 already built.
//!
//! ## What a result may claim
//!
//! A candidate is only ever built from rows that are *current*:
//! - the Resource is ACTIVE (a DELETED tombstone is never a result), and
//! - the Symbol's `resource_revision` equals the Resource's own.
//!
//! A stale Symbol row is not a locator, so it is filtered out in SQL
//! rather than returned with a warning. What the caller is told instead is
//! [`Currentness`]: when `RESOURCE_INDEX` is DIRTY the candidates are still
//! handed over -- deleting them would turn "possibly out of date" into a
//! false zero -- but they are marked not-current.
//!
//! ## What a result must not claim
//!
//! [`StructuralCoverage`] keeps an empty result honest. A Svelte component
//! is indexed as a container only, so "no Symbol in this file" is a
//! statement about coverage, not about the file. Any Resource in a query's
//! scope that the structural index does not fully cover is reported
//! alongside the candidates.
//!
//! Ambiguity is never resolved by picking one: two files named `app.ts`,
//! or two `run` methods, come back as two candidates.
//!
//! Out of scope here: source bodies and range reads (task 11), filesystem
//! text fallback and the full wire status contract (task 12), targeted
//! refresh (task 13), last-valid policy (task 14), and anything Relation
//! (I3) or semantic (I4). Occurrence rows are deliberately not queried:
//! nothing in this task needs them, and reading a CALL_SITE as a call to
//! *something* is exactly the inference I3 owns.

use std::{collections::HashMap, error::Error, fmt, path::Path};

use brainprint_core::{ResourceId, SymbolId};
use rusqlite::{Connection, OptionalExtension, ToSql, params_from_iter};

use crate::{
    component::{self, ComponentError, FreshnessState},
    db::DbOpenError,
    parser::{self, StructuralCapability},
    resource::{self, Resource, ResourceError, ResourceKind, ResourceLanguage, ResourceRole},
    schema,
    structural::{self, StructuralState},
    symbol::{self, Symbol, SymbolError, SymbolKind},
};

/// How many candidates a search returns before it stops collecting. A
/// bound the caller can raise; the point is that a query is never
/// unbounded, and that hitting it is reported ([`Located::truncated`])
/// rather than silently shortening the answer.
pub const DEFAULT_CANDIDATE_LIMIT: usize = 200;

/// Whether what the index says can be claimed to describe the Workspace as
/// it is right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Currentness {
    Current,
    /// The rows are the last published ones, but something is known to
    /// have moved since. They are still returned -- withholding them would
    /// be a false zero -- and simply not called current.
    NotCurrent(NotCurrentReason),
}

impl Currentness {
    #[must_use]
    pub const fn is_current(self) -> bool {
        matches!(self, Self::Current)
    }
}

/// Why a result cannot be claimed current.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotCurrentReason {
    /// `RESOURCE_INDEX` is DIRTY: a change was seen and the inventory has
    /// not been recovered yet (#16 task 5/6).
    ResourceIndexDirty,
    /// No generation has ever published the Resource inventory, so there
    /// is nothing to call current in the first place.
    ResourceIndexNeverPublished,
}

/// Where a result came from. Structural index only at this tier; the
/// bounded filesystem fallback is task 12's, and it will be a second
/// variant rather than an unmarked result mixed in with these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultSource {
    StructuralIndex,
}

/// How much of a Resource the structural index actually covers.
///
/// Derived from the Resource's persisted structural state (#16 task 14)
/// when it has one, and from its dialect otherwise -- a Resource nothing
/// has analyzed yet is still known to be unsupported or container-only by
/// its extension alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuralCoverage {
    /// Whole-file structural analysis applies: the Symbol set for this
    /// Resource is the file's declarations.
    Complete,
    /// The current bytes do not parse cleanly, so what is stored is the
    /// last valid structure rather than the current one. Zero Symbols
    /// here says nothing about the file.
    Partial,
    /// Only the container's own structure is indexed (a Svelte component).
    /// Zero Symbols here says nothing about what the embedded script
    /// declares -- claiming otherwise is the false zero #16 forbids.
    ContainerOnly,
    /// No structural parser covers this Resource at all.
    Unsupported,
    /// A generated artifact with no mapping back to its original. Its
    /// declarations are deliberately not published as source Symbols, so
    /// zero results here is a statement about the mapping.
    GeneratedUnmapped,
}

impl StructuralCoverage {
    #[must_use]
    pub const fn is_complete(self) -> bool {
        matches!(self, Self::Complete)
    }
}

/// A Resource in a query's scope whose structural coverage is incomplete,
/// carried alongside the candidates so an empty candidate list is never
/// read as a complete "not found".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageNote {
    pub resource_id: ResourceId,
    pub path_rel: String,
    pub coverage: StructuralCoverage,
}

/// One located thing, with everything needed to decide how much to trust
/// it: the candidates themselves, whether the selector was an exact
/// identity, how current the rows are, where they came from, and which
/// Resources in scope the index does not fully cover.
///
/// The full `FOUND`/`NOT_FOUND`/`AMBIGUOUS`/... wire contract is task 12's;
/// this is the internal minimum those states will be derived from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located<T> {
    /// Every current match, deterministically ordered. Never narrowed to
    /// one by guessing.
    pub candidates: Vec<T>,
    /// Matches that describe an *older* revision of their Resource: the
    /// last valid structure of a file that no longer parses (#16 task
    /// 14). Kept as labelled evidence, deliberately in their own list --
    /// mixing them into `candidates` would present a stale span as a
    /// current one. Always empty for a Resource locate, whose rows are
    /// either current or a tombstone.
    pub last_valid: Vec<T>,
    /// Whether the selector identified a thing exactly (an id, a full
    /// path, a full qualified name) rather than searching for one.
    pub exact_selector: bool,
    /// Whether [`DEFAULT_CANDIDATE_LIMIT`] (or the query's own limit) cut
    /// the list short.
    pub truncated: bool,
    pub currentness: Currentness,
    pub source: ResultSource,
    pub incomplete_coverage: Vec<CoverageNote>,
}

impl<T> Located<T> {
    /// The single unambiguous answer: an exact selector that matched
    /// exactly once. Two matches are two candidates, never a first pick.
    #[must_use]
    pub fn exact(&self) -> Option<&T> {
        if self.exact_selector && self.candidates.len() == 1 {
            self.candidates.first()
        } else {
            None
        }
    }

    #[must_use]
    pub fn is_ambiguous(&self) -> bool {
        self.candidates.len() > 1
    }
}

/// The current Resource inventory a files/tree/list request asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileListing {
    /// ACTIVE Resources only, ordered by `path_key`.
    pub entries: Vec<Resource>,
    pub truncated: bool,
    pub currentness: Currentness,
    pub source: ResultSource,
}

/// Which part of the inventory to list, and what to keep.
///
/// The defaults list the whole Workspace; every field narrows it.
#[derive(Debug, Clone, Default)]
pub struct FileQuery<'a> {
    /// The directory to list, Workspace-root-relative and without a
    /// trailing slash. `Some("")` is the Workspace root. `None` is the
    /// whole Workspace, which is recursive by definition.
    pub directory: Option<&'a str>,
    /// With a `directory`: whether to descend into its subdirectories.
    /// Without one there is nothing to descend from -- the whole Workspace
    /// is already recursive.
    pub recursive: bool,
    /// Keep only paths starting with this prefix. Composes with
    /// `directory` rather than replacing it.
    pub path_prefix: Option<&'a str>,
    pub role: Option<ResourceRole>,
    pub language: Option<ResourceLanguage>,
    pub kind: Option<ResourceKind>,
    /// `None` uses [`DEFAULT_CANDIDATE_LIMIT`].
    pub limit: Option<usize>,
}

/// How a Resource is named in a locate request.
#[derive(Debug, Clone, Copy)]
pub enum ResourceLocator<'a> {
    /// The stable id: exact by construction.
    Id(ResourceId),
    /// The full Workspace-relative path (`path_rel` == `path_key`).
    Path(&'a str),
    /// The final path segment. Several Resources may share one, and they
    /// all come back.
    Basename(&'a str),
    /// Every Resource under a path prefix.
    PathPrefix(&'a str),
}

/// How a Symbol is named in a locate/search request.
#[derive(Debug, Clone, Copy)]
pub enum SymbolSelector<'a> {
    Id(SymbolId),
    /// The full lexical `qualified_name` within its Resource.
    QualifiedName(&'a str),
    /// The declared name. Not unique: two Resources may each declare
    /// `run`, and both are returned.
    Name(&'a str),
    /// A substring of the declared name. A search, not an identity --
    /// matching is ASCII-case-insensitive substring containment, and no
    /// ranking is invented on top of it.
    PartialName(&'a str),
}

impl SymbolSelector<'_> {
    const fn is_exact(self) -> bool {
        !matches!(self, Self::PartialName(_))
    }
}

/// Which Resources a Symbol search may look in.
#[derive(Debug, Clone, Copy)]
pub enum ResourceScope<'a> {
    Id(ResourceId),
    PathPrefix(&'a str),
}

/// A structured Symbol search.
#[derive(Debug, Clone, Copy)]
pub struct SymbolQuery<'a> {
    pub selector: SymbolSelector<'a>,
    pub scope: Option<ResourceScope<'a>>,
    pub kind: Option<SymbolKind>,
    pub language: Option<ResourceLanguage>,
    /// `None` uses [`DEFAULT_CANDIDATE_LIMIT`].
    pub limit: Option<usize>,
}

impl<'a> SymbolQuery<'a> {
    /// A query with no narrowing beyond the selector itself.
    #[must_use]
    pub const fn new(selector: SymbolSelector<'a>) -> Self {
        Self {
            selector,
            scope: None,
            kind: None,
            language: None,
            limit: None,
        }
    }
}

/// One Symbol candidate: its identity, where it is declared, and how much
/// of its Resource the index covers. Never a source body -- the span is
/// what a read (task 11) would use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolCandidate {
    /// Carries the `SymbolId`, name, kind, `qualified_name`, `ResourceId`,
    /// the exact declaration span, and the parent Symbol's identity.
    pub symbol: Symbol,
    pub path_rel: String,
    pub coverage: StructuralCoverage,
}

/// What declaration a byte position sits inside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionResult {
    /// The smallest current Symbol whose declaration span contains the
    /// position, or `None` when no declaration does -- a top-level
    /// position, which is an answer rather than a miss.
    pub containing: Option<SymbolCandidate>,
    pub coverage: StructuralCoverage,
    pub currentness: Currentness,
    pub source: ResultSource,
}

/// Failure answering a structured query.
#[derive(Debug)]
pub enum QueryError {
    Open(DbOpenError),
    Sqlite(rusqlite::Error),
    Resource(ResourceError),
    Symbol(SymbolError),
    Component(ComponentError),
    /// A locate named a Resource this `index.db` does not have.
    UnknownResource {
        resource_id: ResourceId,
    },
    /// A stored structural state could not be decoded.
    Structural {
        detail: String,
    },
}

impl fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(source) => write!(formatter, "failed to open index.db: {source}"),
            Self::Sqlite(source) => write!(formatter, "query sqlite error: {source}"),
            Self::Resource(source) => write!(formatter, "resource row failed to decode: {source}"),
            Self::Symbol(source) => write!(formatter, "symbol row failed to decode: {source}"),
            Self::Component(source) => write!(formatter, "component state failed: {source}"),
            Self::UnknownResource { resource_id } => {
                write!(formatter, "no resource row for {resource_id}")
            }
            Self::Structural { detail } => {
                write!(formatter, "structural state failed: {detail}")
            }
        }
    }
}

impl Error for QueryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Open(source) => Some(source),
            Self::Sqlite(source) => Some(source),
            Self::Resource(source) => Some(source),
            Self::Symbol(source) => Some(source),
            Self::Component(source) => Some(source),
            Self::UnknownResource { .. } | Self::Structural { .. } => None,
        }
    }
}

impl From<DbOpenError> for QueryError {
    fn from(source: DbOpenError) -> Self {
        Self::Open(source)
    }
}

impl From<rusqlite::Error> for QueryError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

impl From<ResourceError> for QueryError {
    fn from(source: ResourceError) -> Self {
        Self::Resource(source)
    }
}

impl From<SymbolError> for QueryError {
    fn from(source: SymbolError) -> Self {
        Self::Symbol(source)
    }
}

impl From<ComponentError> for QueryError {
    fn from(source: ComponentError) -> Self {
        Self::Component(source)
    }
}

/// Read-only structured access to one Workspace's `index.db`.
pub struct QueryIndex {
    connection: Connection,
}

impl QueryIndex {
    /// Open (creating/migrating if needed) the `index.db` at `path`.
    pub fn open(path: &Path) -> Result<Self, QueryError> {
        let opened = schema::index::open(path)?;
        Ok(Self::from_connection(opened.connection))
    }

    /// Wrap an already-opened `index.db` connection.
    #[must_use]
    pub fn from_connection(connection: Connection) -> Self {
        Self { connection }
    }

    /// The underlying connection, for the read paths built on top of this
    /// index (#16 task 11).
    pub(crate) const fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Whether results read right now may be claimed to describe the
    /// current Workspace.
    pub fn currentness(&self) -> Result<Currentness, QueryError> {
        let Some(state) = component::read(&self.connection)? else {
            return Ok(Currentness::NotCurrent(
                NotCurrentReason::ResourceIndexNeverPublished,
            ));
        };
        Ok(match state.freshness_state {
            FreshnessState::Current => Currentness::Current,
            FreshnessState::Dirty => Currentness::NotCurrent(NotCurrentReason::ResourceIndexDirty),
        })
    }

    /// The current Resource inventory, filtered as `query` asks.
    ///
    /// Reads the `resource` table and nothing else: no directory is
    /// walked, no file is opened. DELETED tombstones are excluded by the
    /// same `state = 'ACTIVE'` predicate every other query here uses.
    pub fn list_files(&self, query: &FileQuery<'_>) -> Result<FileListing, QueryError> {
        let limit = query.limit.unwrap_or(DEFAULT_CANDIDATE_LIMIT);
        let directory_prefix = query.directory.map(directory_prefix);

        let mut sql = format!("{} WHERE r.state = 'ACTIVE'", resource::SELECT_RESOURCE_SQL);
        let mut values: Vec<Box<dyn ToSql>> = Vec::new();
        if let Some(prefix) = directory_prefix.as_deref() {
            push_prefix_filter(&mut sql, &mut values, "r.path_key", prefix);
        }
        if let Some(role) = query.role {
            sql.push_str(" AND r.role = ?");
            values.push(Box::new(role.to_string()));
        }
        if let Some(language) = query.language {
            sql.push_str(" AND r.language = ?");
            values.push(Box::new(language.to_string()));
        }
        if let Some(kind) = query.kind {
            sql.push_str(" AND r.kind = ?");
            values.push(Box::new(kind.to_string()));
        }
        sql.push_str(" ORDER BY r.path_key");

        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(
            params_from_iter(values.iter().map(AsRef::as_ref)),
            resource::raw_resource_from_row,
        )?;

        // The depth and path-prefix predicates are applied here rather
        // than in SQL: both are exact string tests on a row the range
        // filter above already narrowed, and expressing them as LIKE
        // patterns would mean escaping `%`/`_` out of real filenames.
        let mut entries = Vec::new();
        let mut truncated = false;
        for raw in rows {
            let resource = resource::decode_resource(raw?)?;
            if let (Some(prefix), false) = (directory_prefix.as_deref(), query.recursive)
                && !is_direct_child(&resource.path_key, prefix)
            {
                continue;
            }
            if let Some(prefix) = query.path_prefix
                && !resource.path_rel.starts_with(prefix)
            {
                continue;
            }
            if entries.len() == limit {
                truncated = true;
                break;
            }
            entries.push(resource);
        }

        Ok(FileListing {
            entries,
            truncated,
            currentness: self.currentness()?,
            source: ResultSource::StructuralIndex,
        })
    }

    /// Locate a Resource. An id or a full path is exact; a basename or a
    /// path prefix is a candidate search, and several matches stay
    /// several matches.
    pub fn locate_resource(
        &self,
        locator: ResourceLocator<'_>,
    ) -> Result<Located<Resource>, QueryError> {
        let mut sql = format!("{} WHERE r.state = 'ACTIVE'", resource::SELECT_RESOURCE_SQL);
        let mut values: Vec<Box<dyn ToSql>> = Vec::new();
        match locator {
            ResourceLocator::Id(id) => {
                sql.push_str(" AND r.uid = ?");
                values.push(Box::new(id.to_bytes().to_vec()));
            }
            ResourceLocator::Path(path) => {
                sql.push_str(" AND r.path_key = ?");
                values.push(Box::new(path.to_owned()));
            }
            ResourceLocator::Basename(_) => {}
            ResourceLocator::PathPrefix(prefix) => {
                push_prefix_filter(&mut sql, &mut values, "r.path_key", prefix);
            }
        }
        sql.push_str(" ORDER BY r.path_key");

        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(
            params_from_iter(values.iter().map(AsRef::as_ref)),
            resource::raw_resource_from_row,
        )?;

        let mut candidates = Vec::new();
        let mut truncated = false;
        for raw in rows {
            let resource = resource::decode_resource(raw?)?;
            if let ResourceLocator::Basename(basename) = locator
                && basename_of(&resource.path_key) != basename
            {
                continue;
            }
            if candidates.len() == DEFAULT_CANDIDATE_LIMIT {
                truncated = true;
                break;
            }
            candidates.push(resource);
        }

        Ok(Located {
            candidates,
            // A Resource row is current or a tombstone; there is no older
            // revision of it to keep alongside.
            last_valid: Vec::new(),
            exact_selector: matches!(locator, ResourceLocator::Id(_) | ResourceLocator::Path(_)),
            truncated,
            currentness: self.currentness()?,
            source: ResultSource::StructuralIndex,
            // The Resource inventory covers every ACTIVE Resource; only
            // the *structural* index has partial coverage.
            incomplete_coverage: Vec::new(),
        })
    }

    /// Search the structural Symbol index.
    ///
    /// Only Symbols whose `resource_revision` still matches their ACTIVE
    /// Resource's are considered, so a stale row is never handed back as a
    /// current locator. Same-named Symbols all come back; none is picked.
    pub fn search_symbols(
        &self,
        query: &SymbolQuery<'_>,
    ) -> Result<Located<SymbolCandidate>, QueryError> {
        let limit = query.limit.unwrap_or(DEFAULT_CANDIDATE_LIMIT);
        let mut sql = format!(
            "SELECT {}, {RESOURCE_COLUMNS} {} WHERE {}",
            symbol::SYMBOL_COLUMNS,
            symbol::SYMBOL_FROM,
            ACTIVE_RESOURCE_PREDICATE,
        );
        let mut values: Vec<Box<dyn ToSql>> = Vec::new();

        match query.selector {
            SymbolSelector::Id(id) => {
                sql.push_str(" AND s.uid = ?");
                values.push(Box::new(id.to_bytes().to_vec()));
            }
            SymbolSelector::QualifiedName(name) => {
                sql.push_str(" AND s.qualified_name = ?");
                values.push(Box::new(name.to_owned()));
            }
            SymbolSelector::Name(name) => {
                sql.push_str(" AND s.name = ?");
                values.push(Box::new(name.to_owned()));
            }
            SymbolSelector::PartialName(fragment) => {
                // Bounded substring filtering in SQL, with `%`/`_`/`\`
                // escaped so a fragment is a fragment and not a pattern.
                // It is a filter, not a ranking: nothing here claims one
                // match is a better answer than another.
                sql.push_str(r" AND s.name LIKE ? ESCAPE '\'");
                values.push(Box::new(format!("%{}%", escape_like(fragment))));
            }
        }
        match query.scope {
            None => {}
            Some(ResourceScope::Id(id)) => {
                sql.push_str(" AND r.uid = ?");
                values.push(Box::new(id.to_bytes().to_vec()));
            }
            Some(ResourceScope::PathPrefix(prefix)) => {
                push_prefix_filter(&mut sql, &mut values, "r.path_key", prefix);
            }
        }
        if let Some(kind) = query.kind {
            sql.push_str(" AND s.kind = ?");
            values.push(Box::new(kind.as_str()));
        }
        if let Some(language) = query.language {
            sql.push_str(" AND r.language = ?");
            values.push(Box::new(language.to_string()));
        }
        // Deterministic down to the stable id, so equally-placed
        // candidates never swap places between runs.
        sql.push_str(" ORDER BY r.path_key, s.start_byte, s.end_byte DESC, s.name, s.uid");
        sql.push_str(" LIMIT ?");
        values.push(Box::new(
            i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX),
        ));

        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(
            params_from_iter(values.iter().map(AsRef::as_ref)),
            symbol_candidate_row,
        )?;
        let states = self.structural_states()?;
        let mut candidates = Vec::new();
        let mut last_valid = Vec::new();
        let mut truncated = false;
        for row in rows {
            let (raw, path_rel, kind, resource_revision, resource_uid) = row?;
            if candidates.len() == limit {
                truncated = true;
                break;
            }
            let symbol = symbol::decode_symbol(raw)?;
            // A Symbol extracted from an older revision is not a current
            // locator. It is still the last thing this file was known to
            // declare, so it is kept -- in the other list.
            let current = symbol.resource_revision == resource_revision;
            let candidate = SymbolCandidate {
                coverage: coverage_from(&states, &resource_uid, &path_rel, &kind),
                symbol,
                path_rel,
            };
            if current {
                candidates.push(candidate);
            } else {
                last_valid.push(candidate);
            }
        }

        Ok(Located {
            candidates,
            last_valid,
            exact_selector: query.selector.is_exact(),
            truncated,
            currentness: self.currentness()?,
            source: ResultSource::StructuralIndex,
            incomplete_coverage: self.incomplete_coverage(query.scope)?,
        })
    }

    /// The smallest current Symbol whose declaration span contains
    /// `byte_offset` in `resource_id`.
    ///
    /// No AST is re-parsed and no source is read: containment is a
    /// comparison between the position and the spans already stored.
    pub fn symbol_at(
        &self,
        resource_id: ResourceId,
        byte_offset: usize,
    ) -> Result<PositionResult, QueryError> {
        let Some(resource) = self.active_resource(resource_id)? else {
            return Err(QueryError::UnknownResource { resource_id });
        };
        let offset = i64::try_from(byte_offset).unwrap_or(i64::MAX);
        let sql = format!(
            "SELECT {}, {RESOURCE_COLUMNS} {} \
             WHERE {CURRENT_SYMBOL_PREDICATE} AND r.uid = ? \
             AND s.start_byte <= ? AND s.end_byte > ? \
             ORDER BY (s.end_byte - s.start_byte), s.start_byte DESC, s.uid \
             LIMIT 1",
            symbol::SYMBOL_COLUMNS,
            symbol::SYMBOL_FROM,
        );
        let found = self
            .connection
            .query_row(
                &sql,
                rusqlite::params![resource_id.to_bytes().to_vec(), offset, offset],
                symbol_candidate_row,
            )
            .optional()?;

        let coverage = self.coverage_for(&resource)?;
        let containing = found
            .map(|(raw, path_rel, ..)| {
                Ok::<_, QueryError>(SymbolCandidate {
                    symbol: symbol::decode_symbol(raw)?,
                    path_rel,
                    coverage,
                })
            })
            .transpose()?;

        Ok(PositionResult {
            containing,
            coverage,
            currentness: self.currentness()?,
            source: ResultSource::StructuralIndex,
        })
    }

    /// The ACTIVE Resource with this id, if there is one.
    pub fn active_resource(&self, id: ResourceId) -> Result<Option<Resource>, QueryError> {
        let raw = self
            .connection
            .query_row(
                &format!(
                    "{} WHERE r.uid = ?1 AND r.state = 'ACTIVE'",
                    resource::SELECT_RESOURCE_SQL
                ),
                rusqlite::params![id.to_bytes().to_vec()],
                resource::raw_resource_from_row,
            )
            .optional()?;
        Ok(raw.map(resource::decode_resource).transpose()?)
    }

    /// Every Resource's persisted structural state, keyed by scope key.
    /// One query, so a search does not ask per candidate.
    fn structural_states(&self) -> Result<HashMap<String, StructuralState>, QueryError> {
        Ok(structural::read_all(&self.connection)
            .map_err(|error| QueryError::Structural {
                detail: error.to_string(),
            })?
            .into_iter()
            .map(|(key, state)| (key, state.state))
            .collect())
    }

    /// One Resource's coverage, from its persisted structural state when
    /// it has one.
    fn coverage_for(&self, resource: &Resource) -> Result<StructuralCoverage, QueryError> {
        let states = self.structural_states()?;
        Ok(coverage_from(
            &states,
            &resource.id.to_bytes(),
            &resource.path_rel,
            resource.kind.as_str(),
        ))
    }

    /// Every Resource in `scope` the structural index does not fully
    /// cover. Only language-classified files are reported: a `.md` file is
    /// not partial structural coverage, it is simply not code.
    fn incomplete_coverage(
        &self,
        scope: Option<ResourceScope<'_>>,
    ) -> Result<Vec<CoverageNote>, QueryError> {
        let mut sql = format!(
            "{} WHERE r.state = 'ACTIVE' AND r.kind = 'FILE' AND r.language IS NOT NULL",
            resource::SELECT_RESOURCE_SQL
        );
        let mut values: Vec<Box<dyn ToSql>> = Vec::new();
        match scope {
            None => {}
            Some(ResourceScope::Id(id)) => {
                sql.push_str(" AND r.uid = ?");
                values.push(Box::new(id.to_bytes().to_vec()));
            }
            Some(ResourceScope::PathPrefix(prefix)) => {
                push_prefix_filter(&mut sql, &mut values, "r.path_key", prefix);
            }
        }
        sql.push_str(" ORDER BY r.path_key");

        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(
            params_from_iter(values.iter().map(AsRef::as_ref)),
            resource::raw_resource_from_row,
        )?;
        let states = self.structural_states()?;
        let mut notes = Vec::new();
        for raw in rows {
            let resource = resource::decode_resource(raw?)?;
            let coverage = coverage_from(
                &states,
                &resource.id.to_bytes(),
                &resource.path_rel,
                resource.kind.as_str(),
            );
            if !coverage.is_complete() {
                notes.push(CoverageNote {
                    resource_id: resource.id,
                    path_rel: resource.path_rel,
                    coverage,
                });
            }
        }
        Ok(notes)
    }
}

/// What makes a Symbol row current: an ACTIVE Resource, and a revision
/// that still matches that Resource's own. A stale row is not a locator.
const CURRENT_SYMBOL_PREDICATE: &str =
    "r.state = 'ACTIVE' AND s.resource_revision = r.resource_revision";

/// A DELETED Resource's Symbols are never returned at all -- not as a
/// candidate, and not as last-valid evidence. The revision comparison is
/// then made in Rust, so a stale row can be *labelled* rather than
/// dropped (#16 task 14).
const ACTIVE_RESOURCE_PREDICATE: &str = "r.state = 'ACTIVE'";

/// A Symbol row plus the columns appended after `SYMBOL_COLUMNS`: its
/// Resource's path, kind, current revision, and stable id.
type SymbolCandidateRow = (symbol::RawSymbolRow, String, String, String, Vec<u8>);

const RESOURCE_COLUMNS: &str = "r.path_rel, r.kind, r.resource_revision, r.uid";

fn symbol_candidate_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SymbolCandidateRow> {
    let raw = symbol::raw_symbol_row(row)?;
    Ok((raw, row.get(17)?, row.get(18)?, row.get(19)?, row.get(20)?))
}

/// A Resource's coverage: what the structural index recorded about it
/// (#16 task 14) if anything, and otherwise what its extension implies.
///
/// The recorded state is preferred because it is the only thing that can
/// distinguish "this file parses and declares nothing" from "this file
/// stopped parsing and what is stored is last-valid".
fn coverage_from(
    states: &HashMap<String, StructuralState>,
    resource_uid: &[u8],
    path_rel: &str,
    kind: &str,
) -> StructuralCoverage {
    let key = ResourceId::from_bytes(resource_uid.try_into().unwrap_or([0; 16])).to_string();
    match states.get(&key) {
        Some(StructuralState::Complete) => StructuralCoverage::Complete,
        Some(StructuralState::Partial) => StructuralCoverage::Partial,
        Some(StructuralState::ContainerOnly) => StructuralCoverage::ContainerOnly,
        Some(StructuralState::Unsupported | StructuralState::Unavailable) => {
            StructuralCoverage::Unsupported
        }
        Some(StructuralState::GeneratedUnmapped) => StructuralCoverage::GeneratedUnmapped,
        None => coverage_of(path_rel, kind),
    }
}

/// The structural coverage a path's extension implies. Deterministic and
/// offline: the dialect registry decides, nothing is parsed.
pub(crate) fn coverage_of(path_rel: &str, kind: &str) -> StructuralCoverage {
    if kind != ResourceKind::File.as_str() {
        return StructuralCoverage::Unsupported;
    }
    match parser::dialect_for_path(path_rel) {
        Err(_) => StructuralCoverage::Unsupported,
        Ok(dialect) => match dialect.capability() {
            StructuralCapability::WholeFile => StructuralCoverage::Complete,
            StructuralCapability::Container { .. } => StructuralCoverage::ContainerOnly,
        },
    }
}

/// The `path_key` prefix that selects everything inside `directory`. The
/// Workspace root (`""`) selects everything.
fn directory_prefix(directory: &str) -> String {
    let trimmed = directory.trim_end_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}/")
    }
}

/// Whether `path_key` names an entry directly inside `prefix` rather than
/// in one of its subdirectories.
fn is_direct_child(path_key: &str, prefix: &str) -> bool {
    path_key
        .strip_prefix(prefix)
        .is_some_and(|remainder| !remainder.is_empty() && !remainder.contains('/'))
}

fn basename_of(path_key: &str) -> &str {
    path_key.rsplit('/').next().unwrap_or(path_key)
}

/// Add a bounded prefix range on `column`, so SQLite can use the column's
/// index instead of scanning. A prefix whose successor is not valid UTF-8
/// keeps only the lower bound; the exact test still runs in Rust.
fn push_prefix_filter(
    sql: &mut String,
    values: &mut Vec<Box<dyn ToSql>>,
    column: &str,
    prefix: &str,
) {
    if prefix.is_empty() {
        return;
    }
    sql.push_str(&format!(" AND {column} >= ?"));
    values.push(Box::new(prefix.to_owned()));
    if let Some(upper) = prefix_upper_bound(prefix) {
        sql.push_str(&format!(" AND {column} < ?"));
        values.push(Box::new(upper));
    }
}

/// The smallest string strictly greater than every string starting with
/// `prefix`, under SQLite's byte-wise TEXT comparison.
fn prefix_upper_bound(prefix: &str) -> Option<String> {
    let mut bytes = prefix.as_bytes().to_vec();
    while let Some(last) = bytes.pop() {
        if last < 0xFF {
            bytes.push(last + 1);
            return String::from_utf8(bytes).ok();
        }
    }
    None
}

/// Escape a literal fragment for a `LIKE ... ESCAPE '\'` pattern.
fn escape_like(fragment: &str) -> String {
    let mut escaped = String::with_capacity(fragment.len());
    for character in fragment.chars() {
        if matches!(character, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
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
        extract::{assign_ids, extract},
        parser::{ParserRegistry, SourceBasis, dialect_for_resource},
        resource::ResourceStore,
        scan::BaselineScan,
        symbol::SymbolStore,
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    const APP_TS: &str = "\
export class App {
  run(): number {
    return 1
  }
}

function helper(): number { return 2 }
";

    const UTIL_TS: &str = "\
export function run(): number { return 3 }
export const LIMIT = 10
";

    const LIB_PY: &str = "\
def run():
    return 4
";

    const WIDGET_SVELTE: &str = "\
<script lang=\"ts\">
  export function mount() {}
</script>
<div>hi</div>
";

    /// A Workspace with a real baseline: several Resources, duplicate
    /// basenames, a Svelte container, and published Symbols.
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-query-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            fs::create_dir_all(root.join("src").join("util")).expect("src/util");
            fs::create_dir_all(root.join("ui")).expect("ui");
            fs::create_dir_all(root.join("docs")).expect("docs");
            let fixture = Self { base, root };
            fixture.write("src/app.ts", APP_TS);
            fixture.write("src/util/app.ts", UTIL_TS);
            fixture.write("lib.py", LIB_PY);
            fixture.write("ui/Widget.svelte", WIDGET_SVELTE);
            fixture.write("docs/readme.md", "# hi\n");
            fixture
        }

        fn db_path(&self) -> PathBuf {
            self.base.join("data").join("index.db")
        }

        fn write(&self, rel: &str, contents: &str) {
            fs::write(self.root.join(rel), contents).expect("fixture file");
        }

        /// Publish the Resource baseline, then extract and store the
        /// Symbols of every supported source file.
        fn index(&self) -> QueryIndex {
            let engine = BaselineScan::open(&self.db_path()).expect("index.db");
            engine
                .run_initial_scan(&self.root, &WorkspaceConfig::default(), "workspace-rev-1")
                .expect("baseline scan");
            drop(engine);

            let store = SymbolStore::open(&self.db_path()).expect("index.db");
            for (rel, source) in [
                ("src/app.ts", APP_TS),
                ("src/util/app.ts", UTIL_TS),
                ("lib.py", LIB_PY),
                ("ui/Widget.svelte", WIDGET_SVELTE),
            ] {
                let resource = self.resource(rel);
                let dialect = dialect_for_resource(&resource).expect("a supported dialect");
                let mut registry = ParserRegistry::new();
                let tree = registry
                    .parse(dialect, source.as_bytes(), SourceBasis::of(&resource))
                    .expect("parse");
                let extraction = extract(&tree, source.as_bytes());
                if !extraction.is_accepted() {
                    // A Svelte component is CONTAINER_ONLY: nothing is
                    // published for it, which is exactly the zero-Symbol
                    // case coverage has to explain.
                    continue;
                }
                let profile_id = store.ensure_profile(&extraction.profile).expect("profile");
                let symbols = assign_ids(&[], &extraction, &resource, profile_id);
                store
                    .replace_for_resource(resource.id, &resource.resource_revision, &symbols)
                    .expect("replace");
            }
            drop(store);

            QueryIndex::open(&self.db_path()).expect("index.db")
        }

        fn resources(&self) -> ResourceStore {
            ResourceStore::open(&self.db_path()).expect("index.db")
        }

        fn resource(&self, rel: &str) -> Resource {
            self.resources()
                .get_active_by_path_key(rel)
                .expect("lookup")
                .expect("the fixture file is a Resource")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn paths(listing: &FileListing) -> Vec<&str> {
        listing
            .entries
            .iter()
            .map(|entry| entry.path_rel.as_str())
            .collect()
    }

    fn qualified(located: &Located<SymbolCandidate>) -> Vec<(&str, &str)> {
        located
            .candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.path_rel.as_str(),
                    candidate.symbol.qualified_name.as_str(),
                )
            })
            .collect()
    }

    #[test]
    fn root_children_are_answered_from_the_index_with_no_filesystem_left_to_walk() {
        let fixture = Fixture::create("root-children");
        let index = fixture.index();
        // The proof that nothing walks, reads, or parses the Workspace:
        // it is not there any more.
        fs::remove_dir_all(&fixture.root).expect("workspace removed");

        let listing = index
            .list_files(&FileQuery {
                directory: Some(""),
                ..FileQuery::default()
            })
            .expect("list");

        assert_eq!(paths(&listing), vec!["docs", "lib.py", "src", "ui"]);
        assert!(listing.currentness.is_current());
        assert_eq!(listing.source, ResultSource::StructuralIndex);
    }

    #[test]
    fn directory_children_recursion_and_path_prefix_narrow_the_listing() {
        let fixture = Fixture::create("directory");
        let index = fixture.index();

        let children = index
            .list_files(&FileQuery {
                directory: Some("src"),
                ..FileQuery::default()
            })
            .expect("list");
        assert_eq!(paths(&children), vec!["src/app.ts", "src/util"]);

        let recursive = index
            .list_files(&FileQuery {
                directory: Some("src"),
                recursive: true,
                ..FileQuery::default()
            })
            .expect("list");
        assert_eq!(
            paths(&recursive),
            vec!["src/app.ts", "src/util", "src/util/app.ts"]
        );

        let whole = index.list_files(&FileQuery::default()).expect("list");
        assert_eq!(
            paths(&whole),
            vec![
                "docs",
                "docs/readme.md",
                "lib.py",
                "src",
                "src/app.ts",
                "src/util",
                "src/util/app.ts",
                "ui",
                "ui/Widget.svelte",
            ],
            "the default listing is the whole inventory, ordered deterministically"
        );

        let prefixed = index
            .list_files(&FileQuery {
                path_prefix: Some("src/util"),
                ..FileQuery::default()
            })
            .expect("list");
        assert_eq!(paths(&prefixed), vec!["src/util", "src/util/app.ts"]);
    }

    #[test]
    fn role_language_and_kind_filters_use_the_stored_classification() {
        let fixture = Fixture::create("filters");
        let index = fixture.index();

        let docs = index
            .list_files(&FileQuery {
                role: Some(ResourceRole::Docs),
                ..FileQuery::default()
            })
            .expect("list");
        assert_eq!(paths(&docs), vec!["docs/readme.md"]);

        let python = index
            .list_files(&FileQuery {
                language: Some(ResourceLanguage::Python),
                ..FileQuery::default()
            })
            .expect("list");
        assert_eq!(paths(&python), vec!["lib.py"]);

        let directories = index
            .list_files(&FileQuery {
                kind: Some(ResourceKind::Directory),
                ..FileQuery::default()
            })
            .expect("list");
        assert_eq!(paths(&directories), vec!["docs", "src", "src/util", "ui"]);
    }

    #[test]
    fn a_deleted_resource_leaves_the_current_listing_and_locate_results() {
        let fixture = Fixture::create("deleted");
        let index = fixture.index();
        let doomed = fixture.resource("lib.py");
        fixture
            .resources()
            .mark_deleted(doomed.id, "rev-deleted")
            .expect("delete");

        let listing = index.list_files(&FileQuery::default()).expect("list");
        assert!(
            !paths(&listing).contains(&"lib.py"),
            "a DELETED tombstone is not part of the current inventory"
        );
        assert!(
            index
                .locate_resource(ResourceLocator::Id(doomed.id))
                .expect("locate")
                .candidates
                .is_empty()
        );
        let surviving = index
            .search_symbols(&SymbolQuery::new(SymbolSelector::Name("run")))
            .expect("search");
        assert_eq!(
            qualified(&surviving),
            vec![("src/app.ts", "App.run"), ("src/util/app.ts", "run")],
            "a deleted Resource's Symbols are not current locators"
        );
    }

    #[test]
    fn a_resource_is_located_exactly_by_stable_id_and_by_path() {
        let fixture = Fixture::create("resource-exact");
        let index = fixture.index();
        let app = fixture.resource("src/app.ts");

        let by_id = index
            .locate_resource(ResourceLocator::Id(app.id))
            .expect("locate");
        assert_eq!(by_id.exact().map(|found| found.id), Some(app.id));

        let by_path = index
            .locate_resource(ResourceLocator::Path("src/app.ts"))
            .expect("locate");
        assert_eq!(by_path.exact().map(|found| found.id), Some(app.id));
        assert!(!by_path.is_ambiguous());

        let missing = index
            .locate_resource(ResourceLocator::Path("src/nope.ts"))
            .expect("locate");
        assert!(missing.candidates.is_empty() && missing.exact().is_none());
    }

    #[test]
    fn a_duplicate_basename_returns_every_candidate_and_picks_none() {
        let fixture = Fixture::create("duplicate-basename");
        let index = fixture.index();

        let located = index
            .locate_resource(ResourceLocator::Basename("app.ts"))
            .expect("locate");

        let found: Vec<&str> = located
            .candidates
            .iter()
            .map(|candidate| candidate.path_rel.as_str())
            .collect();
        assert_eq!(found, vec!["src/app.ts", "src/util/app.ts"]);
        assert!(located.is_ambiguous());
        assert!(
            located.exact().is_none(),
            "a basename search never resolves to one arbitrary file"
        );
    }

    #[test]
    fn a_symbol_is_located_exactly_by_stable_id_and_by_qualified_name() {
        let fixture = Fixture::create("symbol-exact");
        let index = fixture.index();

        let by_qualified = index
            .search_symbols(&SymbolQuery::new(SymbolSelector::QualifiedName("App.run")))
            .expect("search");
        let found = by_qualified.exact().expect("exactly one App.run");
        assert_eq!(found.symbol.kind, SymbolKind::Method);
        assert_eq!(found.path_rel, "src/app.ts");
        assert_eq!(found.coverage, StructuralCoverage::Complete);

        let by_id = index
            .search_symbols(&SymbolQuery::new(SymbolSelector::Id(found.symbol.id)))
            .expect("search");
        assert_eq!(
            by_id.exact().map(|candidate| candidate.symbol.id),
            Some(found.symbol.id)
        );
        assert_eq!(
            by_id.exact().map(|candidate| candidate.symbol.parent_id),
            Some(found.symbol.parent_id),
            "the containing declaration's identity travels with the candidate"
        );
    }

    #[test]
    fn same_named_symbols_stay_several_candidates() {
        let fixture = Fixture::create("same-name");
        let index = fixture.index();

        let located = index
            .search_symbols(&SymbolQuery::new(SymbolSelector::Name("run")))
            .expect("search");

        assert_eq!(
            qualified(&located),
            vec![
                ("lib.py", "run"),
                ("src/app.ts", "App.run"),
                ("src/util/app.ts", "run"),
            ]
        );
        assert!(located.is_ambiguous());
        assert!(
            located.exact().is_none(),
            "three declarations named run are three candidates"
        );
    }

    #[test]
    fn a_partial_name_search_matches_substrings_without_reading_source() {
        let fixture = Fixture::create("partial");
        let index = fixture.index();
        fs::remove_dir_all(&fixture.root).expect("workspace removed");

        let located = index
            .search_symbols(&SymbolQuery::new(SymbolSelector::PartialName("elp")))
            .expect("search");

        assert_eq!(qualified(&located), vec![("src/app.ts", "helper")]);
        assert!(
            !located.exact_selector,
            "a substring search is not an exact identity"
        );
    }

    #[test]
    fn kind_path_scope_and_language_narrow_a_symbol_search() {
        let fixture = Fixture::create("narrowing");
        let index = fixture.index();

        let methods = index
            .search_symbols(&SymbolQuery {
                kind: Some(SymbolKind::Method),
                ..SymbolQuery::new(SymbolSelector::Name("run"))
            })
            .expect("search");
        assert_eq!(qualified(&methods), vec![("src/app.ts", "App.run")]);

        let scoped = index
            .search_symbols(&SymbolQuery {
                scope: Some(ResourceScope::PathPrefix("src/util")),
                ..SymbolQuery::new(SymbolSelector::Name("run"))
            })
            .expect("search");
        assert_eq!(qualified(&scoped), vec![("src/util/app.ts", "run")]);

        let by_resource = index
            .search_symbols(&SymbolQuery {
                scope: Some(ResourceScope::Id(fixture.resource("src/app.ts").id)),
                ..SymbolQuery::new(SymbolSelector::PartialName("r"))
            })
            .expect("search");
        assert_eq!(
            qualified(&by_resource),
            vec![("src/app.ts", "App.run"), ("src/app.ts", "helper")]
        );

        let python = index
            .search_symbols(&SymbolQuery {
                language: Some(ResourceLanguage::Python),
                ..SymbolQuery::new(SymbolSelector::Name("run"))
            })
            .expect("search");
        assert_eq!(qualified(&python), vec![("lib.py", "run")]);
    }

    #[test]
    fn a_position_resolves_to_the_smallest_containing_declaration() {
        let fixture = Fixture::create("position");
        let index = fixture.index();
        let app = fixture.resource("src/app.ts");
        let inside_run = APP_TS.find("return 1").expect("body offset");

        let found = index.symbol_at(app.id, inside_run).expect("position");

        let containing = found.containing.expect("a declaration contains it");
        assert_eq!(
            containing.symbol.qualified_name, "App.run",
            "the innermost declaration wins over the class around it"
        );
        let declaration =
            &APP_TS[containing.symbol.span.start_byte..containing.symbol.span.end_byte];
        assert!(
            declaration.starts_with("run(): number {") && declaration.ends_with('}'),
            "the stored span is the exact declaration: {declaration:?}"
        );
        assert_eq!(found.coverage, StructuralCoverage::Complete);
    }

    #[test]
    fn a_position_no_declaration_contains_is_an_answer_not_a_miss() {
        let fixture = Fixture::create("position-top-level");
        let index = fixture.index();
        let app = fixture.resource("src/app.ts");
        let between = APP_TS.find("\n\nfunction").expect("gap offset");

        let found = index.symbol_at(app.id, between).expect("position");

        assert!(
            found.containing.is_none(),
            "a top-level position has no containing Symbol, which is not a failure"
        );
        assert_eq!(found.coverage, StructuralCoverage::Complete);
        assert!(found.currentness.is_current());
    }

    #[test]
    fn a_symbol_whose_resource_revision_moved_on_is_not_a_current_locator() {
        let fixture = Fixture::create("stale");
        let index = fixture.index();
        let mut app = fixture.resource("src/app.ts");
        app.resource_revision = "rev-moved-on".to_owned();
        assert!(fixture.resources().update_resource(&app).expect("update"));

        let located = index
            .search_symbols(&SymbolQuery::new(SymbolSelector::QualifiedName("App.run")))
            .expect("search");

        assert!(
            located.candidates.is_empty(),
            "a Symbol extracted from a revision the Resource no longer has is stale"
        );
        assert!(
            index
                .symbol_at(app.id, APP_TS.find("return 1").expect("offset"))
                .expect("position")
                .containing
                .is_none()
        );
    }

    #[test]
    fn a_dirty_resource_index_keeps_the_candidates_and_marks_them_not_current() {
        let fixture = Fixture::create("dirty");
        let index = fixture.index();
        component::mark_dirty(&index.connection, "workspace-rev-1", Some("TEST"))
            .expect("mark dirty");

        let located = index
            .search_symbols(&SymbolQuery::new(SymbolSelector::QualifiedName("App.run")))
            .expect("search");
        let listing = index.list_files(&FileQuery::default()).expect("list");

        assert_eq!(located.candidates.len(), 1, "candidates are not discarded");
        assert_eq!(
            located.currentness,
            Currentness::NotCurrent(NotCurrentReason::ResourceIndexDirty)
        );
        assert!(!listing.entries.is_empty());
        assert_eq!(
            listing.currentness,
            Currentness::NotCurrent(NotCurrentReason::ResourceIndexDirty)
        );
    }

    #[test]
    fn a_container_only_resource_with_no_symbols_is_not_a_complete_not_found() {
        let fixture = Fixture::create("container-only");
        let index = fixture.index();
        let widget = fixture.resource("ui/Widget.svelte");

        // #19 task 11 made the component's script real, so what it
        // declares is found.
        assert!(
            !index
                .search_symbols(&SymbolQuery {
                    scope: Some(ResourceScope::Id(widget.id)),
                    ..SymbolQuery::new(SymbolSelector::Name("mount"))
                })
                .expect("search")
                .candidates
                .is_empty(),
            "the component's script declares `mount`"
        );

        // And a name it does not declare is still a coverage statement
        // rather than a complete not-found, because a component is more
        // than its script.
        let located = index
            .search_symbols(&SymbolQuery {
                scope: Some(ResourceScope::Id(widget.id)),
                ..SymbolQuery::new(SymbolSelector::Name("never_declared"))
            })
            .expect("search");

        assert!(located.candidates.is_empty());
        assert_eq!(
            located.incomplete_coverage,
            vec![CoverageNote {
                resource_id: widget.id,
                path_rel: "ui/Widget.svelte".to_owned(),
                coverage: StructuralCoverage::ContainerOnly,
            }],
            "the empty result is explained by coverage, not reported as complete"
        );
        assert_eq!(
            index.symbol_at(widget.id, 30).expect("position").coverage,
            StructuralCoverage::ContainerOnly
        );
    }

    #[test]
    fn querying_creates_no_relation_or_resolution_rows() {
        let fixture = Fixture::create("no-relation");
        let index = fixture.index();

        index.list_files(&FileQuery::default()).expect("list");
        index
            .locate_resource(ResourceLocator::Basename("app.ts"))
            .expect("locate");
        index
            .search_symbols(&SymbolQuery::new(SymbolSelector::PartialName("run")))
            .expect("search");
        index
            .symbol_at(fixture.resource("src/app.ts").id, 10)
            .expect("position");

        for table in [
            "relation",
            "unresolved_reference",
            "graph_entity",
            "relation_candidate",
            "resolution_context",
        ] {
            let count: i64 = index
                .connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .expect("count");
            assert_eq!(count, 0, "{table} must stay empty at this tier");
        }
    }
}
