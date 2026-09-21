//! Canonical GraphEntity/Relation model and its `index.db` storage
//! primitives (#17 task 1 / #2 relation model / #13 task 5).
//!
//! I2 published *what is there*: Resources, Symbols, and the Occurrence
//! evidence about them. A Relation is the other thing -- an interpreted
//! connection between two endpoints. This module is only the vocabulary
//! and the storage boundary for those connections. It resolves nothing,
//! extracts nothing, and reads no source.
//!
//! ## Endpoints, not row ids
//!
//! `graph_entity` is the canonical wrapper that lets a Relation point at
//! a Resource, a Symbol, an external package entity, or a domain entity
//! without four sets of columns. Its row id is a storage detail and never
//! leaves this module: every public signature speaks
//! [`GraphEndpoint`], which carries the stable `ResourceId`/`SymbolId`
//! that I1/I2 already own, or the natural key of an external/domain
//! entity. The existing partial unique indexes make one endpoint one
//! entity row, so "ensure" is idempotent by construction rather than by a
//! read-then-write race.
//!
//! ## One direction, one row
//!
//! There is no `CALLED_BY`. A reverse question is answered by reading the
//! same rows through `idx_relation_target_kind`, which is what that index
//! is for. Storing the mirror would double every write and create a way
//! for the two halves to disagree.
//!
//! ## Canonical identity
//!
//! An edge is identified by `(kind, source, target)` and nothing else
//! (#17 task 2). [`Dispatch`] is an attribute of the edge, not part of
//! its name: "A calls B" does not become two facts because one call site
//! binds statically and another does not, and per-site detail is
//! Occurrence evidence. [`TargetScope`] is a function of the target
//! entity, so it cannot discriminate between edges either -- and because
//! it is derived rather than supplied, a caller cannot label an external
//! package INTERNAL. `idx_relation_identity` (migration 5) is exactly
//! that tuple, so the runtime [`RelationKey`] and the database agree by
//! construction rather than by convention.
//!
//! Neither axis is ever NULL on a write: "not known" is
//! [`Dispatch::Unknown`], which is a value, so the same logical edge can
//! never exist twice under two spellings of "no information".
//!
//! A `relation` row means [`Resolution::Resolved`]. Candidates and
//! unresolved references are different tables with a different lifecycle
//! (#17 task 7); nothing here writes a relation to mean "maybe".
//!
//! ## What this tier deliberately does not do
//!
//! Occurrence linkage is #17 task 3; resolution, candidates and
//! unresolved references are tasks 2 and 7; extraction is task 4 onward;
//! traversal is task 10. `dispatch` and `target_scope` are carried
//! verbatim here because they participate in the row's uniqueness -- task
//! 2 owns their closed vocabularies, and inventing one now would mean
//! writing it twice. No source text is stored: a Relation is two
//! endpoints and a kind.

use std::{error::Error, fmt, path::Path};

use brainprint_core::{ResourceId, SymbolId};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    db::DbOpenError,
    resolution::{Dispatch, Resolution, ResolutionContext, TargetScope, UnknownAxisValue},
    schema,
};

/// The P0 canonical relation kinds (#17 "Relation 의미").
///
/// A closed vocabulary: an unrecognized stored value is a decode error,
/// never a silent fallback. Reverse kinds are deliberately absent --
/// direction is a property of the row, not of the kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelationKind {
    Calls,
    References,
    Imports,
    Extends,
    Implements,
    Overrides,
    UsesType,
}

impl RelationKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Calls => "CALLS",
            Self::References => "REFERENCES",
            Self::Imports => "IMPORTS",
            Self::Extends => "EXTENDS",
            Self::Implements => "IMPLEMENTS",
            Self::Overrides => "OVERRIDES",
            Self::UsesType => "USES_TYPE",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, GraphError> {
        match raw {
            "CALLS" => Ok(Self::Calls),
            "REFERENCES" => Ok(Self::References),
            "IMPORTS" => Ok(Self::Imports),
            "EXTENDS" => Ok(Self::Extends),
            "IMPLEMENTS" => Ok(Self::Implements),
            "OVERRIDES" => Ok(Self::Overrides),
            "USES_TYPE" => Ok(Self::UsesType),
            other => Err(GraphError::UnknownRelationKind {
                raw: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for RelationKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Which of the four canonical targets a `graph_entity` wraps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    Resource,
    Symbol,
    External,
    Domain,
}

impl EntityKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resource => "RESOURCE",
            Self::Symbol => "SYMBOL",
            Self::External => "EXTERNAL",
            Self::Domain => "DOMAIN",
        }
    }

    fn parse(raw: &str) -> Result<Self, GraphError> {
        match raw {
            "RESOURCE" => Ok(Self::Resource),
            "SYMBOL" => Ok(Self::Symbol),
            "EXTERNAL" => Ok(Self::External),
            "DOMAIN" => Ok(Self::Domain),
            other => Err(GraphError::UnknownEntityKind {
                raw: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for EntityKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// An entity outside the Workspace: a package, and optionally the module
/// and symbol within it.
///
/// The minimum typed boundary the existing schema allows -- its natural
/// key is `(package_identity, module_path, symbol_name)`, which is what
/// makes "the same external thing" the same entity. `kind` stays open
/// text at this tier: no extractor produces one yet (#17 task 4), and a
/// vocabulary invented before its first producer is a guess.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExternalEntity {
    pub package_identity: String,
    pub module_path: Option<String>,
    pub symbol_name: Option<String>,
    pub qualified_name: Option<String>,
    pub kind: String,
    pub resolved_version: Option<String>,
    /// Where the declaration can be found, if that is known. A locator,
    /// never source text.
    pub declaration_locator: Option<String>,
}

/// A domain-level entity: an environment variable, a config key, and
/// whatever #17 task 12 adds. Natural key
/// `(kind, normalized_identity, namespace, method)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DomainEntity {
    pub kind: String,
    pub normalized_identity: String,
    pub namespace: Option<String>,
    pub method: Option<String>,
    pub display_label: String,
}

/// What a Relation points at, in stable identity terms.
///
/// This is the whole public identity surface of the graph: a caller never
/// sees, passes, or stores a `graph_entity.id`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GraphEndpoint {
    Resource(ResourceId),
    Symbol(SymbolId),
    External(ExternalEntity),
    Domain(DomainEntity),
}

impl GraphEndpoint {
    #[must_use]
    pub const fn entity_kind(&self) -> EntityKind {
        match self {
            Self::Resource(_) => EntityKind::Resource,
            Self::Symbol(_) => EntityKind::Symbol,
            Self::External(_) => EntityKind::External,
            Self::Domain(_) => EntityKind::Domain,
        }
    }
}

/// One `graph_entity` row, as identity rather than as storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphEntity {
    pub kind: EntityKind,
    pub endpoint: GraphEndpoint,
}

/// One canonical, resolved edge.
///
/// There is no `resolution` field: a stored relation is
/// [`Resolution::Resolved`] by definition, and there is deliberately no
/// way to write one that means anything else.
///
/// There is no `target_scope` field either. It is derived from the
/// target endpoint ([`Self::target_scope`]), which is what keeps a
/// caller from declaring an external package INTERNAL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    pub kind: RelationKind,
    pub source: GraphEndpoint,
    pub target: GraphEndpoint,
    /// How the source binds to the target. [`Dispatch::Unknown`] when an
    /// extractor has not established it -- never NULL.
    pub dispatch: Dispatch,
    /// The generation that first published this edge.
    pub created_generation: i64,
}

impl Relation {
    /// Always [`Resolution::Resolved`]. A relation row is a confirmed
    /// edge; a possible one is a candidate, and it lives elsewhere.
    #[must_use]
    pub const fn resolution(&self) -> Resolution {
        Resolution::Resolved
    }

    /// Derived from the target endpoint: a Resource, Symbol, or domain
    /// entity is in this Workspace; an external package is not.
    #[must_use]
    pub const fn target_scope(&self) -> TargetScope {
        target_scope_of(&self.target)
    }

    /// This edge's canonical identity.
    #[must_use]
    pub const fn key(&self) -> RelationKey<'_> {
        RelationKey {
            kind: self.kind,
            source: &self.source,
            target: &self.target,
        }
    }
}

/// The scope a target endpoint implies. Deterministic, and the only way
/// `relation.target_scope` is ever written.
#[must_use]
pub const fn target_scope_of(target: &GraphEndpoint) -> TargetScope {
    match target {
        GraphEndpoint::Resource(_) | GraphEndpoint::Symbol(_) | GraphEndpoint::Domain(_) => {
            TargetScope::Internal
        }
        GraphEndpoint::External(_) => TargetScope::External,
    }
}

/// What identifies one edge -- the same tuple `idx_relation_identity`
/// enforces, so runtime and storage cannot drift apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationKey<'a> {
    pub kind: RelationKind,
    pub source: &'a GraphEndpoint,
    pub target: &'a GraphEndpoint,
}

/// Failure at the graph storage boundary.
#[derive(Debug)]
pub enum GraphError {
    Open(DbOpenError),
    Sqlite(rusqlite::Error),
    UnknownRelationKind {
        raw: String,
    },
    UnknownEntityKind {
        raw: String,
    },
    /// A stored `dispatch`/`target_scope` outside its closed vocabulary.
    UnknownAxisValue(UnknownAxisValue),
    /// A stored `target_scope` disagrees with what its target endpoint
    /// is. The column is derived on every write, so this is corruption,
    /// not a case to paper over.
    TargetScopeMismatch {
        stored: TargetScope,
        derived: TargetScope,
    },
    /// A `graph_entity` row claims a kind whose payload column is NULL,
    /// or vice versa. The schema's CHECK makes this unreachable through
    /// this module; reading it is a corruption report, not a fallback.
    MalformedEntity {
        kind: EntityKind,
    },
    /// An endpoint named a `ResourceId` this `index.db` does not have.
    UnknownResource {
        resource_id: ResourceId,
    },
    /// An endpoint named a `SymbolId` this `index.db` does not have.
    UnknownSymbol {
        symbol_id: SymbolId,
    },
    /// A relation named an endpoint that has no `graph_entity` yet.
    /// Entities are ensured explicitly, so that an edge can never bring a
    /// half-described endpoint into existence as a side effect.
    UnknownEntity {
        endpoint: Box<GraphEndpoint>,
    },
}

impl fmt::Display for GraphError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(source) => write!(formatter, "failed to open index.db: {source}"),
            Self::Sqlite(source) => write!(formatter, "graph store sqlite error: {source}"),
            Self::UnknownRelationKind { raw } => {
                write!(formatter, "unknown relation kind {raw:?}")
            }
            Self::UnknownEntityKind { raw } => write!(formatter, "unknown entity kind {raw:?}"),
            Self::UnknownAxisValue(source) => write!(formatter, "{source}"),
            Self::TargetScopeMismatch { stored, derived } => write!(
                formatter,
                "stored target scope {stored} is not the {derived} its target endpoint implies"
            ),
            Self::MalformedEntity { kind } => {
                write!(
                    formatter,
                    "graph entity of kind {kind} has no {kind} payload"
                )
            }
            Self::UnknownResource { resource_id } => {
                write!(formatter, "no resource row for {resource_id}")
            }
            Self::UnknownSymbol { symbol_id } => {
                write!(formatter, "no symbol row for {symbol_id}")
            }
            Self::UnknownEntity { endpoint } => write!(
                formatter,
                "no graph entity for {:?}; ensure it before relating it",
                endpoint.entity_kind()
            ),
        }
    }
}

impl Error for GraphError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Open(source) => Some(source),
            Self::Sqlite(source) => Some(source),
            _ => None,
        }
    }
}

impl From<DbOpenError> for GraphError {
    fn from(source: DbOpenError) -> Self {
        Self::Open(source)
    }
}

impl From<rusqlite::Error> for GraphError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

impl From<UnknownAxisValue> for GraphError {
    fn from(source: UnknownAxisValue) -> Self {
        Self::UnknownAxisValue(source)
    }
}

/// Typed access to one Workspace's `graph_entity` and `relation` tables.
///
/// Storage primitives only. The Resource-owned replacement that #17 task
/// 3 builds on top of these runs in the caller's transaction, which is
/// why every write here is a single statement with no transaction of its
/// own.
pub struct GraphStore {
    connection: Connection,
}

impl GraphStore {
    /// Open (creating/migrating if needed) the `index.db` at `path`.
    pub fn open(path: &Path) -> Result<Self, GraphError> {
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

    /// The `graph_entity` for this endpoint, creating it if this is the
    /// first time the endpoint is used.
    ///
    /// Idempotent: the schema's partial unique indexes mean one endpoint
    /// has one entity row, so calling this twice returns the same entity
    /// rather than making a second one.
    pub fn ensure_entity(&self, endpoint: &GraphEndpoint) -> Result<GraphEntity, GraphError> {
        ensure_entity(&self.connection, endpoint)
    }

    /// The `graph_entity` for this endpoint, or `None` if it has never
    /// been ensured.
    pub fn entity(&self, endpoint: &GraphEndpoint) -> Result<Option<GraphEntity>, GraphError> {
        Ok(entity_id(&self.connection, endpoint)?.map(|_| GraphEntity {
            kind: endpoint.entity_kind(),
            endpoint: endpoint.clone(),
        }))
    }

    /// How many entities this graph holds. For tests and diagnostics.
    pub fn entity_count(&self) -> Result<i64, GraphError> {
        Ok(self
            .connection
            .query_row("SELECT COUNT(*) FROM graph_entity", [], |row| row.get(0))?)
    }

    /// The `resolution_context` row for this context, creating it on
    /// first use, and returning its deterministic key.
    ///
    /// Same evidence, same key, same row: a context is identified by
    /// what it fingerprints, so a second call with equal fingerprints
    /// reuses the row rather than accumulating near-duplicates. Only
    /// fingerprints are stored -- never the config, lockfile, or source
    /// they were computed from.
    pub fn ensure_resolution_context(
        &self,
        context: &ResolutionContext,
        created_generation: i64,
    ) -> Result<String, GraphError> {
        ensure_resolution_context(&self.connection, context, created_generation)
    }

    /// The context behind a key, or `None`.
    pub fn resolution_context(
        &self,
        context_key: &str,
    ) -> Result<Option<ResolutionContext>, GraphError> {
        resolution_context(&self.connection, context_key)
    }

    /// How many resolution contexts this index holds.
    pub fn resolution_context_count(&self) -> Result<i64, GraphError> {
        Ok(self
            .connection
            .query_row("SELECT COUNT(*) FROM resolution_context", [], |row| {
                row.get(0)
            })?)
    }

    /// Insert one edge, or report that the schema already had it.
    ///
    /// Both endpoints must already exist ([`Self::ensure_entity`]): an
    /// edge is never allowed to conjure an endpoint, because a half
    /// described endpoint is exactly what a later resolution pass would
    /// then have to guess about.
    pub fn insert_relation(&self, relation: &Relation) -> Result<bool, GraphError> {
        insert_relation(&self.connection, relation)
    }

    /// One edge by its identity, or `None`.
    pub fn relation(&self, key: &RelationKey<'_>) -> Result<Option<Relation>, GraphError> {
        relation(&self.connection, key)
    }

    /// Remove one edge. `false` if it was not there.
    pub fn delete_relation(&self, key: &RelationKey<'_>) -> Result<bool, GraphError> {
        delete_relation(&self.connection, key)
    }

    /// Remove every edge leaving `source`, returning how many went.
    ///
    /// The primitive an owner-scoped replacement is built from (#17 task
    /// 3). What "owner" means -- which Resource's re-analysis may delete
    /// which edges -- is that task's contract, not this one's.
    pub fn delete_relations_from(&self, source: &GraphEndpoint) -> Result<usize, GraphError> {
        delete_relations_from(&self.connection, source)
    }

    /// Edges leaving `source`, optionally narrowed to one kind.
    pub fn relations_from(
        &self,
        source: &GraphEndpoint,
        kind: Option<RelationKind>,
    ) -> Result<Vec<Relation>, GraphError> {
        relations_by(&self.connection, Direction::From, source, kind)
    }

    /// Edges arriving at `target`, optionally narrowed to one kind.
    ///
    /// This is how a reverse question is answered: the same rows, read
    /// through the target index. No `CALLED_BY` row exists to read.
    pub fn relations_to(
        &self,
        target: &GraphEndpoint,
        kind: Option<RelationKind>,
    ) -> Result<Vec<Relation>, GraphError> {
        relations_by(&self.connection, Direction::To, target, kind)
    }

    /// How many edges this graph holds.
    pub fn relation_count(&self) -> Result<i64, GraphError> {
        Ok(self
            .connection
            .query_row("SELECT COUNT(*) FROM relation", [], |row| row.get(0))?)
    }

    /// A transaction on this store's connection, for a caller that must
    /// commit graph rows together with other `index.db` rows.
    pub fn transaction(&self) -> Result<rusqlite::Transaction<'_>, GraphError> {
        Ok(self.connection.unchecked_transaction()?)
    }
}

// The `&Connection` functions below are what [`GraphStore`]'s methods are
// built from, following the same pattern as `generation`, `component`,
// and `symbol`: #17 task 3's Resource-owned replacement must write these
// rows in the *same* transaction as the Occurrences they belong to, and
// the caller owns that connection.

pub(crate) fn ensure_entity(
    connection: &Connection,
    endpoint: &GraphEndpoint,
) -> Result<GraphEntity, GraphError> {
    let column = payload_column(endpoint);
    let payload = payload_id(connection, endpoint, true)?;
    if entity_id(connection, endpoint)?.is_none() {
        connection.execute(
            &format!(
                "INSERT INTO graph_entity (entity_kind, {column}) VALUES (?1, ?2) \
                 ON CONFLICT DO NOTHING"
            ),
            params![endpoint.entity_kind().as_str(), payload],
        )?;
    }
    Ok(GraphEntity {
        kind: endpoint.entity_kind(),
        endpoint: endpoint.clone(),
    })
}

pub(crate) fn ensure_resolution_context(
    connection: &Connection,
    context: &ResolutionContext,
    created_generation: i64,
) -> Result<String, GraphError> {
    let key = context.context_key();
    connection.execute(
        "INSERT INTO resolution_context \
         (context_key, language, scope_key, config_fingerprint, dependency_fingerprint, \
          environment_fingerprint, module_resolution_fingerprint, backend_snapshot_token, \
          created_generation) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
         ON CONFLICT (context_key) DO NOTHING",
        params![
            key,
            context.language,
            context.scope_key,
            context.config_fingerprint,
            context.dependency_fingerprint,
            context.environment_fingerprint,
            context.module_resolution_fingerprint,
            context.backend_snapshot_token,
            created_generation,
        ],
    )?;
    Ok(key)
}

pub(crate) fn resolution_context(
    connection: &Connection,
    context_key: &str,
) -> Result<Option<ResolutionContext>, GraphError> {
    Ok(connection
        .query_row(
            "SELECT language, scope_key, config_fingerprint, dependency_fingerprint, \
                    environment_fingerprint, module_resolution_fingerprint, \
                    backend_snapshot_token \
             FROM resolution_context WHERE context_key = ?1",
            params![context_key],
            |row| {
                Ok(ResolutionContext {
                    language: row.get(0)?,
                    scope_key: row.get(1)?,
                    config_fingerprint: row.get(2)?,
                    dependency_fingerprint: row.get(3)?,
                    environment_fingerprint: row.get(4)?,
                    module_resolution_fingerprint: row.get(5)?,
                    backend_snapshot_token: row.get(6)?,
                })
            },
        )
        .optional()?)
}

pub(crate) fn insert_relation(
    connection: &Connection,
    relation: &Relation,
) -> Result<bool, GraphError> {
    let source = require_entity(connection, &relation.source)?;
    let target = require_entity(connection, &relation.target)?;
    // Both axes are written from values, never left NULL, and the scope
    // comes from the target endpoint rather than from the caller.
    let changed = connection.execute(
        "INSERT INTO relation \
         (kind, source_entity_id, target_entity_id, dispatch, target_scope, created_generation) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT DO NOTHING",
        params![
            relation.kind.as_str(),
            source,
            target,
            relation.dispatch.as_str(),
            relation.target_scope().as_str(),
            relation.created_generation,
        ],
    )?;
    Ok(changed == 1)
}

pub(crate) fn relation(
    connection: &Connection,
    key: &RelationKey<'_>,
) -> Result<Option<Relation>, GraphError> {
    let Some(source) = entity_id(connection, key.source)? else {
        return Ok(None);
    };
    let Some(target) = entity_id(connection, key.target)? else {
        return Ok(None);
    };
    let raw: Option<RawRelationRow> = connection
        .query_row(
            "SELECT kind, source_entity_id, target_entity_id, dispatch, target_scope, \
                    created_generation \
             FROM relation \
             WHERE kind = ?1 AND source_entity_id = ?2 AND target_entity_id = ?3",
            params![key.kind.as_str(), source, target],
            raw_relation_row,
        )
        .optional()?;
    raw.map(|raw| decode_relation(connection, raw)).transpose()
}

pub(crate) fn delete_relation(
    connection: &Connection,
    key: &RelationKey<'_>,
) -> Result<bool, GraphError> {
    let (Some(source), Some(target)) = (
        entity_id(connection, key.source)?,
        entity_id(connection, key.target)?,
    ) else {
        return Ok(false);
    };
    let changed = connection.execute(
        "DELETE FROM relation \
         WHERE kind = ?1 AND source_entity_id = ?2 AND target_entity_id = ?3",
        params![key.kind.as_str(), source, target],
    )?;
    Ok(changed == 1)
}

pub(crate) fn delete_relations_from(
    connection: &Connection,
    source: &GraphEndpoint,
) -> Result<usize, GraphError> {
    let Some(source) = entity_id(connection, source)? else {
        return Ok(0);
    };
    Ok(connection.execute(
        "DELETE FROM relation WHERE source_entity_id = ?1",
        params![source],
    )?)
}

/// Which end of the edge a lookup is anchored on.
#[derive(Debug, Clone, Copy)]
enum Direction {
    From,
    To,
}

impl Direction {
    const fn column(self) -> &'static str {
        match self {
            Self::From => "source_entity_id",
            Self::To => "target_entity_id",
        }
    }
}

fn relations_by(
    connection: &Connection,
    direction: Direction,
    endpoint: &GraphEndpoint,
    kind: Option<RelationKind>,
) -> Result<Vec<Relation>, GraphError> {
    let Some(anchor) = entity_id(connection, endpoint)? else {
        return Ok(Vec::new());
    };
    let mut sql = format!(
        "SELECT kind, source_entity_id, target_entity_id, dispatch, target_scope, \
                created_generation \
         FROM relation WHERE {} = ?1",
        direction.column()
    );
    if kind.is_some() {
        sql.push_str(" AND kind = ?2");
    }
    // Deterministic, and it is the stored direction that orders the
    // result -- there is no second, mirrored row to interleave.
    sql.push_str(" ORDER BY kind, source_entity_id, target_entity_id");

    let mut statement = connection.prepare(&sql)?;
    let rows = match kind {
        Some(kind) => statement.query_map(params![anchor, kind.as_str()], raw_relation_row)?,
        None => statement.query_map(params![anchor], raw_relation_row)?,
    };
    rows.map(|raw| decode_relation(connection, raw?)).collect()
}

type RawRelationRow = (String, i64, i64, Option<String>, Option<String>, i64);

fn raw_relation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRelationRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}

fn decode_relation(connection: &Connection, raw: RawRelationRow) -> Result<Relation, GraphError> {
    let target = endpoint_of(connection, raw.2)?;
    // A row written before #17 task 2 could still hold NULL; migration 5
    // normalizes those, so a NULL here means somebody wrote around this
    // module rather than that the value is unknown.
    let dispatch = Dispatch::parse(raw.3.as_deref().unwrap_or_default())?;
    let derived = target_scope_of(&target);
    if let Some(stored) = raw.4.as_deref() {
        let stored = TargetScope::parse(stored)?;
        if stored != derived {
            return Err(GraphError::TargetScopeMismatch { stored, derived });
        }
    }
    Ok(Relation {
        kind: RelationKind::parse(&raw.0)?,
        source: endpoint_of(connection, raw.1)?,
        target,
        dispatch,
        created_generation: raw.5,
    })
}

/// `graph_entity`'s columns as stored: the kind plus its four mutually
/// exclusive payload columns, of which the schema's CHECK guarantees
/// exactly one is set.
type RawEntityRow = (String, Option<i64>, Option<i64>, Option<i64>, Option<i64>);

/// The endpoint one `graph_entity` row stands for. The row id goes in;
/// only stable identity comes out.
fn endpoint_of(connection: &Connection, entity_id: i64) -> Result<GraphEndpoint, GraphError> {
    let (kind, resource, symbol, external, domain): RawEntityRow = connection.query_row(
        "SELECT entity_kind, resource_id, symbol_id, external_entity_id, domain_entity_id \
         FROM graph_entity WHERE id = ?1",
        params![entity_id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    let kind = EntityKind::parse(&kind)?;
    match kind {
        EntityKind::Resource => {
            let local = resource.ok_or(GraphError::MalformedEntity { kind })?;
            let uid: Vec<u8> = connection.query_row(
                "SELECT uid FROM resource WHERE id = ?1",
                params![local],
                |row| row.get(0),
            )?;
            Ok(GraphEndpoint::Resource(ResourceId::from_bytes(
                stable_bytes(&uid),
            )))
        }
        EntityKind::Symbol => {
            let local = symbol.ok_or(GraphError::MalformedEntity { kind })?;
            let uid: Vec<u8> = connection.query_row(
                "SELECT uid FROM symbol WHERE id = ?1",
                params![local],
                |row| row.get(0),
            )?;
            Ok(GraphEndpoint::Symbol(SymbolId::from_bytes(stable_bytes(
                &uid,
            ))))
        }
        EntityKind::External => {
            let local = external.ok_or(GraphError::MalformedEntity { kind })?;
            connection
                .query_row(
                    "SELECT package_identity, module_path, symbol_name, qualified_name, kind, \
                            resolved_version, declaration_locator \
                     FROM external_entity WHERE id = ?1",
                    params![local],
                    |row| {
                        Ok(GraphEndpoint::External(ExternalEntity {
                            package_identity: row.get(0)?,
                            module_path: row.get(1)?,
                            symbol_name: row.get(2)?,
                            qualified_name: row.get(3)?,
                            kind: row.get(4)?,
                            resolved_version: row.get(5)?,
                            declaration_locator: row.get(6)?,
                        }))
                    },
                )
                .map_err(GraphError::from)
        }
        EntityKind::Domain => {
            let local = domain.ok_or(GraphError::MalformedEntity { kind })?;
            connection
                .query_row(
                    "SELECT kind, normalized_identity, namespace, method, display_label \
                     FROM domain_entity WHERE id = ?1",
                    params![local],
                    |row| {
                        Ok(GraphEndpoint::Domain(DomainEntity {
                            kind: row.get(0)?,
                            normalized_identity: row.get(1)?,
                            namespace: row.get(2)?,
                            method: row.get(3)?,
                            display_label: row.get(4)?,
                        }))
                    },
                )
                .map_err(GraphError::from)
        }
    }
}

fn require_entity(connection: &Connection, endpoint: &GraphEndpoint) -> Result<i64, GraphError> {
    entity_id(connection, endpoint)?.ok_or_else(|| GraphError::UnknownEntity {
        endpoint: Box::new(endpoint.clone()),
    })
}

fn entity_id(connection: &Connection, endpoint: &GraphEndpoint) -> Result<Option<i64>, GraphError> {
    let Some(payload) = payload_id(connection, endpoint, false)? else {
        return Ok(None);
    };
    entity_row_by_payload(connection, endpoint, payload)
}

fn entity_row_by_payload(
    connection: &Connection,
    endpoint: &GraphEndpoint,
    payload: i64,
) -> Result<Option<i64>, GraphError> {
    Ok(connection
        .query_row(
            &format!(
                "SELECT id FROM graph_entity WHERE {} = ?1",
                payload_column(endpoint)
            ),
            params![payload],
            |row| row.get(0),
        )
        .optional()?)
}

const fn payload_column(endpoint: &GraphEndpoint) -> &'static str {
    match endpoint {
        GraphEndpoint::Resource(_) => "resource_id",
        GraphEndpoint::Symbol(_) => "symbol_id",
        GraphEndpoint::External(_) => "external_entity_id",
        GraphEndpoint::Domain(_) => "domain_entity_id",
    }
}

/// The local row id of whatever the endpoint wraps.
///
/// A Resource or a Symbol must already exist: the graph points at I1/I2's
/// identities, it does not create them. An external or domain entity is
/// this module's own to create, so `create` inserts it on its natural key
/// when it is not there yet.
fn payload_id(
    connection: &Connection,
    endpoint: &GraphEndpoint,
    create: bool,
) -> Result<Option<i64>, GraphError> {
    match endpoint {
        GraphEndpoint::Resource(id) => {
            let found: Option<i64> = connection
                .query_row(
                    "SELECT id FROM resource WHERE uid = ?1",
                    params![id.to_bytes().to_vec()],
                    |row| row.get(0),
                )
                .optional()?;
            match (found, create) {
                (Some(found), _) => Ok(Some(found)),
                (None, true) => Err(GraphError::UnknownResource { resource_id: *id }),
                (None, false) => Ok(None),
            }
        }
        GraphEndpoint::Symbol(id) => {
            let found: Option<i64> = connection
                .query_row(
                    "SELECT id FROM symbol WHERE uid = ?1",
                    params![id.to_bytes().to_vec()],
                    |row| row.get(0),
                )
                .optional()?;
            match (found, create) {
                (Some(found), _) => Ok(Some(found)),
                (None, true) => Err(GraphError::UnknownSymbol { symbol_id: *id }),
                (None, false) => Ok(None),
            }
        }
        GraphEndpoint::External(external) => {
            if create {
                connection.execute(
                    "INSERT INTO external_entity \
                     (package_identity, module_path, symbol_name, qualified_name, kind, \
                      resolved_version, declaration_locator) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                     ON CONFLICT (package_identity, module_path, symbol_name) DO NOTHING",
                    params![
                        external.package_identity,
                        external.module_path,
                        external.symbol_name,
                        external.qualified_name,
                        external.kind,
                        external.resolved_version,
                        external.declaration_locator,
                    ],
                )?;
            }
            Ok(connection
                .query_row(
                    "SELECT id FROM external_entity \
                     WHERE package_identity = ?1 AND module_path IS ?2 AND symbol_name IS ?3",
                    params![
                        external.package_identity,
                        external.module_path,
                        external.symbol_name
                    ],
                    |row| row.get(0),
                )
                .optional()?)
        }
        GraphEndpoint::Domain(domain) => {
            if create {
                connection.execute(
                    "INSERT INTO domain_entity \
                     (kind, normalized_identity, namespace, method, display_label) \
                     VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT (kind, normalized_identity, namespace, method) DO NOTHING",
                    params![
                        domain.kind,
                        domain.normalized_identity,
                        domain.namespace,
                        domain.method,
                        domain.display_label,
                    ],
                )?;
            }
            Ok(connection
                .query_row(
                    "SELECT id FROM domain_entity \
                     WHERE kind = ?1 AND normalized_identity = ?2 AND namespace IS ?3 \
                       AND method IS ?4",
                    params![
                        domain.kind,
                        domain.normalized_identity,
                        domain.namespace,
                        domain.method
                    ],
                    |row| row.get(0),
                )
                .optional()?)
        }
    }
}

/// A stored uid is 16 bytes. A stored value that is not is corruption,
/// and zero-filling keeps the read honest rather than panicking on a row
/// no writer in this crate can produce.
fn stable_bytes(raw: &[u8]) -> [u8; 16] {
    raw.try_into().unwrap_or([0; 16])
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
        config::WorkspaceConfig, generation, resource::ResourceStore, scan::BaselineScan,
        symbol::SymbolStore,
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    const APP_TS: &str = "\
export class App {
  run(): number {
    return helper()
  }
}

function helper(): number {
  return 41
}
";

    const LIB_TS: &str = "export function helper2(): number {\n  return 2\n}\n";

    /// A Workspace with a published baseline, so the graph has real
    /// `ResourceId`s and `SymbolId`s to point at.
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-graph-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            fs::create_dir_all(root.join("src")).expect("src");
            let fixture = Self { base, root };
            fixture.write("src/app.ts", APP_TS);
            fixture.write("src/lib.ts", LIB_TS);
            let engine = BaselineScan::open(&fixture.db_path()).expect("index.db");
            engine
                .run_initial_scan(
                    &fixture.root,
                    &WorkspaceConfig::default(),
                    "workspace-rev-1",
                )
                .expect("baseline scan");
            fixture
        }

        fn db_path(&self) -> PathBuf {
            self.base.join("data").join("index.db")
        }

        fn write(&self, rel: &str, contents: &str) {
            fs::write(self.root.join(rel), contents).expect("fixture file");
        }

        fn store(&self) -> GraphStore {
            GraphStore::open(&self.db_path()).expect("index.db")
        }

        fn resource(&self, rel: &str) -> ResourceId {
            ResourceStore::open(&self.db_path())
                .expect("index.db")
                .get_active_by_path_key(rel)
                .expect("lookup")
                .expect("the fixture file is a Resource")
                .id
        }

        fn symbol(&self, rel: &str, qualified_name: &str) -> SymbolId {
            SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_for_resource(self.resource(rel))
                .expect("symbols")
                .into_iter()
                .find(|symbol| symbol.qualified_name == qualified_name)
                .expect("the declaration is indexed")
                .id
        }

        fn generation(&self) -> i64 {
            let store = self.store();
            generation::current_stable(store.connection())
                .expect("stable")
                .expect("the baseline published one")
                .id
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn external() -> ExternalEntity {
        ExternalEntity {
            package_identity: "npm:left-pad@1.3.0".to_owned(),
            module_path: Some("left-pad".to_owned()),
            symbol_name: Some("leftPad".to_owned()),
            qualified_name: Some("left-pad.leftPad".to_owned()),
            kind: "FUNCTION".to_owned(),
            resolved_version: Some("1.3.0".to_owned()),
            declaration_locator: Some("node_modules/left-pad/index.js".to_owned()),
        }
    }

    fn domain() -> DomainEntity {
        DomainEntity {
            kind: "ENV_VAR".to_owned(),
            normalized_identity: "DATABASE_URL".to_owned(),
            namespace: None,
            method: None,
            display_label: "DATABASE_URL".to_owned(),
        }
    }

    #[test]
    fn a_resource_endpoint_round_trips_and_is_never_duplicated() {
        let fixture = Fixture::create("resource-entity");
        let store = fixture.store();
        let endpoint = GraphEndpoint::Resource(fixture.resource("src/app.ts"));

        assert!(store.entity(&endpoint).expect("entity").is_none());
        let created = store.ensure_entity(&endpoint).expect("ensure");
        assert_eq!(created.kind, EntityKind::Resource);
        assert_eq!(created.endpoint, endpoint);

        let again = store.ensure_entity(&endpoint).expect("ensure again");
        assert_eq!(again, created);
        assert_eq!(
            store.entity_count().expect("count"),
            1,
            "one canonical endpoint is one graph_entity"
        );
        assert_eq!(
            store.entity(&endpoint).expect("entity"),
            Some(created),
            "and it reads back as the same stable identity"
        );
    }

    #[test]
    fn a_symbol_endpoint_round_trips_and_is_never_duplicated() {
        let fixture = Fixture::create("symbol-entity");
        let store = fixture.store();
        let endpoint = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "App.run"));

        let created = store.ensure_entity(&endpoint).expect("ensure");
        store.ensure_entity(&endpoint).expect("ensure again");
        assert_eq!(created.kind, EntityKind::Symbol);
        assert_eq!(store.entity_count().expect("count"), 1);
        assert_eq!(store.entity(&endpoint).expect("entity"), Some(created));

        // A different Symbol is a different endpoint, not a reuse.
        let other = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "helper"));
        store.ensure_entity(&other).expect("ensure");
        assert_eq!(store.entity_count().expect("count"), 2);
    }

    #[test]
    fn external_and_domain_endpoints_reuse_their_natural_key() {
        let fixture = Fixture::create("external-domain");
        let store = fixture.store();
        let package = GraphEndpoint::External(external());
        let variable = GraphEndpoint::Domain(domain());

        store.ensure_entity(&package).expect("ensure external");
        store
            .ensure_entity(&package)
            .expect("ensure external again");
        store.ensure_entity(&variable).expect("ensure domain");
        store.ensure_entity(&variable).expect("ensure domain again");

        assert_eq!(store.entity_count().expect("count"), 2);
        assert_eq!(
            store.entity(&package).expect("entity").map(|e| e.endpoint),
            Some(package)
        );
        assert_eq!(
            store.entity(&variable).expect("entity").map(|e| e.endpoint),
            Some(variable)
        );
    }

    #[test]
    fn an_endpoint_the_index_does_not_have_is_refused_explicitly() {
        let fixture = Fixture::create("unknown-endpoint");
        let store = fixture.store();

        let unknown_resource = GraphEndpoint::Resource(ResourceId::generate());
        assert!(matches!(
            store
                .ensure_entity(&unknown_resource)
                .expect_err("a graph entity never invents a Resource"),
            GraphError::UnknownResource { .. }
        ));
        let unknown_symbol = GraphEndpoint::Symbol(SymbolId::generate());
        assert!(matches!(
            store
                .ensure_entity(&unknown_symbol)
                .expect_err("a graph entity never invents a Symbol"),
            GraphError::UnknownSymbol { .. }
        ));
        assert_eq!(store.entity_count().expect("count"), 0);

        // Nor does an edge conjure an endpoint as a side effect.
        let source = GraphEndpoint::Resource(fixture.resource("src/app.ts"));
        store.ensure_entity(&source).expect("ensure");
        let target = GraphEndpoint::Resource(fixture.resource("src/lib.ts"));
        let failure = store
            .insert_relation(&Relation {
                kind: RelationKind::Imports,
                source,
                target,
                dispatch: Dispatch::Unknown,
                created_generation: fixture.generation(),
            })
            .expect_err("the target was never ensured");
        assert!(matches!(failure, GraphError::UnknownEntity { .. }));
        assert_eq!(store.relation_count().expect("count"), 0);
    }

    #[test]
    fn relation_kind_is_a_closed_vocabulary_with_no_reverse_kinds() {
        for kind in [
            RelationKind::Calls,
            RelationKind::References,
            RelationKind::Imports,
            RelationKind::Extends,
            RelationKind::Implements,
            RelationKind::Overrides,
            RelationKind::UsesType,
        ] {
            assert_eq!(
                RelationKind::parse(kind.as_str()).expect("round trip"),
                kind
            );
        }
        for absent in ["CALLED_BY", "IMPORTED_BY", "REFERENCED_BY", "calls", ""] {
            assert!(
                matches!(
                    RelationKind::parse(absent),
                    Err(GraphError::UnknownRelationKind { .. })
                ),
                "{absent:?} must not decode: direction is the row's, not the kind's"
            );
        }
    }

    #[test]
    fn the_same_edge_is_stored_once_and_keeps_its_direction() {
        let fixture = Fixture::create("edge");
        let store = fixture.store();
        let caller = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "App.run"));
        let callee = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "helper"));
        store.ensure_entity(&caller).expect("ensure");
        store.ensure_entity(&callee).expect("ensure");
        let edge = Relation {
            kind: RelationKind::Calls,
            source: caller.clone(),
            target: callee.clone(),
            dispatch: Dispatch::Unknown,
            created_generation: fixture.generation(),
        };

        assert!(store.insert_relation(&edge).expect("insert"));
        assert!(
            !store.insert_relation(&edge).expect("insert again"),
            "the schema's uniqueness is the contract, and it is honoured"
        );
        assert_eq!(store.relation_count().expect("count"), 1);

        // Direction is preserved, and there is exactly one row for it.
        assert_eq!(
            store.relations_from(&caller, None).expect("from"),
            vec![edge.clone()]
        );
        assert!(
            store
                .relations_from(&callee, None)
                .expect("from")
                .is_empty(),
            "the callee calls nothing"
        );
        assert_eq!(
            store.relations_to(&callee, None).expect("to"),
            vec![edge.clone()],
            "the reverse question is the same row read the other way"
        );
        assert!(store.relations_to(&caller, None).expect("to").is_empty());

        // No mirrored CALLED_BY row was created to answer that.
        assert_eq!(store.relation_count().expect("count"), 1);
        let stored_kinds: Vec<String> = {
            let mut statement = store
                .connection()
                .prepare("SELECT kind FROM relation")
                .expect("prepare");
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .expect("query");
            rows.map(|row| row.expect("row")).collect()
        };
        assert_eq!(stored_kinds, vec!["CALLS".to_owned()]);

        // Lookup and delete by the same identity the schema uses.
        let key = RelationKey {
            kind: RelationKind::Calls,
            source: &caller,
            target: &callee,
        };
        assert_eq!(store.relation(&key).expect("get"), Some(edge));
        assert!(store.delete_relation(&key).expect("delete"));
        assert!(!store.delete_relation(&key).expect("delete again"));
        assert_eq!(store.relation_count().expect("count"), 0);
    }

    #[test]
    fn kind_narrowing_and_source_scoped_deletion_are_available_as_primitives() {
        let fixture = Fixture::create("primitives");
        let store = fixture.store();
        let generation = fixture.generation();
        let source = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "App.run"));
        let callee = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "helper"));
        let module = GraphEndpoint::Resource(fixture.resource("src/lib.ts"));
        let package = GraphEndpoint::External(external());
        for endpoint in [&source, &callee, &module, &package] {
            store.ensure_entity(endpoint).expect("ensure");
        }
        for (kind, target) in [
            (RelationKind::Calls, &callee),
            (RelationKind::Imports, &module),
            (RelationKind::References, &package),
        ] {
            assert!(
                store
                    .insert_relation(&Relation {
                        kind,
                        source: source.clone(),
                        target: target.clone(),
                        dispatch: Dispatch::Unknown,
                        created_generation: generation,
                    })
                    .expect("insert")
            );
        }

        assert_eq!(
            store
                .relations_from(&source, Some(RelationKind::Calls))
                .expect("from")
                .len(),
            1
        );
        assert_eq!(store.relations_from(&source, None).expect("from").len(), 3);
        assert_eq!(
            store
                .relations_to(&package, Some(RelationKind::References))
                .expect("to")
                .len(),
            1
        );

        // The primitive an owner-scoped replacement is built from.
        assert_eq!(
            store.delete_relations_from(&source).expect("delete"),
            3,
            "every edge leaving the source goes"
        );
        assert_eq!(store.relation_count().expect("count"), 0);
        assert_eq!(
            store.entity_count().expect("count"),
            4,
            "the endpoints themselves are not deleted by an edge sweep"
        );
    }

    #[test]
    fn reopening_the_index_reuses_the_same_endpoints() {
        let fixture = Fixture::create("reopen");
        let caller = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "App.run"));
        let callee = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "helper"));
        let edge = Relation {
            kind: RelationKind::Calls,
            source: caller.clone(),
            target: callee.clone(),
            dispatch: Dispatch::Unknown,
            created_generation: fixture.generation(),
        };
        {
            let store = fixture.store();
            store.ensure_entity(&caller).expect("ensure");
            store.ensure_entity(&callee).expect("ensure");
            assert!(store.insert_relation(&edge).expect("insert"));
        }

        let reopened = fixture.store();
        reopened.ensure_entity(&caller).expect("ensure");
        assert_eq!(
            reopened.entity_count().expect("count"),
            2,
            "a reopened index reuses the endpoint instead of making a second one"
        );
        assert_eq!(
            reopened.relations_from(&caller, None).expect("from"),
            vec![edge]
        );
    }

    #[test]
    fn each_workspace_has_its_own_graph() {
        let first = Fixture::create("isolation-a");
        let second = Fixture::create("isolation-b");
        let first_store = first.store();
        let second_store = second.store();

        let caller = GraphEndpoint::Symbol(first.symbol("src/app.ts", "App.run"));
        let callee = GraphEndpoint::Symbol(first.symbol("src/app.ts", "helper"));
        first_store.ensure_entity(&caller).expect("ensure");
        first_store.ensure_entity(&callee).expect("ensure");
        assert!(
            first_store
                .insert_relation(&Relation {
                    kind: RelationKind::Calls,
                    source: caller.clone(),
                    target: callee,
                    dispatch: Dispatch::Unknown,
                    created_generation: first.generation(),
                })
                .expect("insert")
        );

        assert_eq!(second_store.entity_count().expect("count"), 0);
        assert_eq!(second_store.relation_count().expect("count"), 0);
        assert!(
            second_store.entity(&caller).expect("entity").is_none(),
            "the other Workspace's SymbolId is not an endpoint here"
        );
        assert!(
            second_store
                .relations_from(&caller, None)
                .expect("from")
                .is_empty()
        );
    }

    #[test]
    fn a_new_edge_never_stores_a_null_axis_and_derives_its_scope() {
        let fixture = Fixture::create("canonical-axes");
        let store = fixture.store();
        let source = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "App.run"));
        let internal = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "helper"));
        let package = GraphEndpoint::External(external());
        let variable = GraphEndpoint::Domain(domain());
        for endpoint in [&source, &internal, &package, &variable] {
            store.ensure_entity(endpoint).expect("ensure");
        }
        for (kind, target, dispatch) in [
            (RelationKind::Calls, &internal, Dispatch::Static),
            (RelationKind::References, &package, Dispatch::Unknown),
            (RelationKind::UsesType, &variable, Dispatch::Dynamic),
        ] {
            assert!(
                store
                    .insert_relation(&Relation {
                        kind,
                        source: source.clone(),
                        target: target.clone(),
                        dispatch,
                        created_generation: fixture.generation(),
                    })
                    .expect("insert")
            );
        }

        // No NULL survives a write, whatever the extractor knew.
        let nulls: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM relation WHERE dispatch IS NULL OR target_scope IS NULL",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(nulls, 0, "\"not known\" is UNKNOWN, not NULL");

        // The scope is the target's, not the caller's.
        let scopes: Vec<(String, String)> = {
            let mut statement = store
                .connection()
                .prepare("SELECT kind, target_scope FROM relation ORDER BY kind")
                .expect("prepare");
            let rows = statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .expect("query");
            rows.map(|row| row.expect("row")).collect()
        };
        assert_eq!(
            scopes,
            vec![
                ("CALLS".to_owned(), "INTERNAL".to_owned()),
                ("REFERENCES".to_owned(), "EXTERNAL".to_owned()),
                ("USES_TYPE".to_owned(), "INTERNAL".to_owned()),
            ],
            "an external package is EXTERNAL; a Resource, Symbol or domain entity is INTERNAL"
        );
        assert_eq!(target_scope_of(&package), TargetScope::External);
        assert_eq!(target_scope_of(&variable), TargetScope::Internal);
        assert_eq!(
            store
                .relations_from(&source, Some(RelationKind::Calls))
                .expect("from")[0]
                .dispatch,
            Dispatch::Static,
            "the dispatch an extractor did establish is preserved"
        );
    }

    #[test]
    fn dispatch_is_an_attribute_and_never_a_second_edge() {
        let fixture = Fixture::create("identity");
        let store = fixture.store();
        let source = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "App.run"));
        let target = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "helper"));
        store.ensure_entity(&source).expect("ensure");
        store.ensure_entity(&target).expect("ensure");
        let edge = |dispatch| Relation {
            kind: RelationKind::Calls,
            source: source.clone(),
            target: target.clone(),
            dispatch,
            created_generation: fixture.generation(),
        };

        assert!(
            store
                .insert_relation(&edge(Dispatch::Unknown))
                .expect("insert")
        );
        // The same logical edge, seen again with more known about how it
        // binds. That is one fact, not two rows.
        assert!(
            !store
                .insert_relation(&edge(Dispatch::Static))
                .expect("insert")
        );
        assert!(
            !store
                .insert_relation(&edge(Dispatch::Dynamic))
                .expect("insert")
        );
        assert_eq!(store.relation_count().expect("count"), 1);

        // Runtime identity and the database agree: the key has no
        // dispatch in it, and looking up by it finds the row whatever
        // dispatch was offered.
        let key = RelationKey {
            kind: RelationKind::Calls,
            source: &source,
            target: &target,
        };
        assert_eq!(key, edge(Dispatch::Dynamic).key());
        let stored = store.relation(&key).expect("get").expect("stored");
        assert_eq!(stored.dispatch, Dispatch::Unknown);
        assert_eq!(stored.resolution(), Resolution::Resolved);
        assert_eq!(stored.target_scope(), TargetScope::Internal);
    }

    #[test]
    fn a_legacy_null_row_is_normalized_by_migration_rather_than_read_as_unknown() {
        let fixture = Fixture::create("backfill");
        let store = fixture.store();
        let source = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "App.run"));
        let internal = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "helper"));
        let package = GraphEndpoint::External(external());
        for endpoint in [&source, &internal, &package] {
            store.ensure_entity(endpoint).expect("ensure");
        }
        let source_id = entity_id(store.connection(), &source)
            .expect("lookup")
            .expect("ensured");
        let internal_id = entity_id(store.connection(), &internal)
            .expect("lookup")
            .expect("ensured");
        let external_id = entity_id(store.connection(), &package)
            .expect("lookup")
            .expect("ensured");
        let generation = fixture.generation();

        // Rows as a pre-task-2 writer could have left them: NULL axes,
        // and two rows for one logical edge because NULL never collided.
        store
            .connection()
            .execute_batch(&format!(
                "DROP INDEX idx_relation_identity; \
                 DROP INDEX idx_relation_unique_null_dispatch; \
                 INSERT INTO relation \
                   (kind, source_entity_id, target_entity_id, dispatch, target_scope, \
                    created_generation) \
                 VALUES ('CALLS', {source_id}, {internal_id}, NULL, NULL, {generation}), \
                        ('CALLS', {source_id}, {internal_id}, NULL, NULL, {generation}), \
                        ('IMPORTS', {source_id}, {external_id}, NULL, NULL, {generation});"
            ))
            .expect("legacy rows");

        // Exactly what migration 5 does, replayed on those rows.
        store
            .connection()
            .execute_batch(crate::schema::index::INDEX_MIGRATIONS[4].sql)
            .expect("backfill");

        let calls = store
            .relation(&RelationKey {
                kind: RelationKind::Calls,
                source: &source,
                target: &internal,
            })
            .expect("get")
            .expect("one row survived");
        assert_eq!(calls.dispatch, Dispatch::Unknown);
        assert_eq!(calls.target_scope(), TargetScope::Internal);
        let imports = store
            .relation(&RelationKey {
                kind: RelationKind::Imports,
                source: &source,
                target: &package,
            })
            .expect("get")
            .expect("stored");
        assert_eq!(imports.target_scope(), TargetScope::External);
        assert_eq!(
            store.relation_count().expect("count"),
            2,
            "the duplicate that only NULL made possible is gone"
        );

        let stored: Vec<(String, String)> = {
            let mut statement = store
                .connection()
                .prepare("SELECT dispatch, target_scope FROM relation")
                .expect("prepare");
            let rows = statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .expect("query");
            rows.map(|row| row.expect("row")).collect()
        };
        assert_eq!(
            stored,
            vec![
                ("UNKNOWN".to_owned(), "INTERNAL".to_owned()),
                ("UNKNOWN".to_owned(), "EXTERNAL".to_owned()),
            ],
            "the backfilled scope comes from what each target actually is"
        );
    }

    #[test]
    fn a_resolution_context_is_reused_by_its_evidence_and_stores_no_config_body() {
        let fixture = Fixture::create("resolution-context");
        let store = fixture.store();
        let generation = fixture.generation();
        let context = ResolutionContext {
            language: "TYPESCRIPT".to_owned(),
            scope_key: "tsconfig.json".to_owned(),
            config_fingerprint: "sha256:config".to_owned(),
            dependency_fingerprint: "sha256:deps".to_owned(),
            environment_fingerprint: "sha256:env".to_owned(),
            module_resolution_fingerprint: "sha256:module".to_owned(),
            backend_snapshot_token: None,
        };

        let key = store
            .ensure_resolution_context(&context, generation)
            .expect("ensure");
        let again = store
            .ensure_resolution_context(&context, generation)
            .expect("ensure again");
        assert_eq!(key, again);
        assert_eq!(
            store.resolution_context_count().expect("count"),
            1,
            "the same evidence is the same context"
        );
        assert_eq!(
            store.resolution_context(&key).expect("read"),
            Some(context.clone())
        );

        // A moved dependency fingerprint is a different context, not the
        // same one updated in place.
        let bumped = ResolutionContext {
            dependency_fingerprint: "sha256:deps-2".to_owned(),
            ..context.clone()
        };
        let bumped_key = store
            .ensure_resolution_context(&bumped, generation)
            .expect("ensure");
        assert_ne!(bumped_key, key);
        assert_eq!(store.resolution_context_count().expect("count"), 2);
        assert_eq!(
            store.resolution_context(&key).expect("read"),
            Some(context),
            "and the original is untouched"
        );
    }

    #[test]
    fn a_relation_row_is_only_ever_a_resolved_edge() {
        let fixture = Fixture::create("resolved-only");
        let store = fixture.store();
        let source = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "App.run"));
        let target = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "helper"));
        store.ensure_entity(&source).expect("ensure");
        store.ensure_entity(&target).expect("ensure");
        let edge = Relation {
            kind: RelationKind::Calls,
            source,
            target,
            dispatch: Dispatch::Unknown,
            created_generation: fixture.generation(),
        };
        assert!(store.insert_relation(&edge).expect("insert"));

        assert_eq!(edge.resolution(), Resolution::Resolved);
        // A maybe belongs in the tables that model a maybe. This tier
        // writes neither, and offers no way to store a relation that
        // means anything but RESOLVED.
        for table in ["unresolved_reference", "relation_candidate"] {
            let count: i64 = store
                .connection()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .expect("count");
            assert_eq!(count, 0, "{table} is #17 task 7's, not this tier's");
        }
        // And there is no confidence column to compress the axes into.
        let columns: Vec<String> = {
            let mut statement = store
                .connection()
                .prepare("SELECT name FROM pragma_table_info('relation')")
                .expect("prepare");
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .expect("query");
            rows.map(|row| row.expect("row")).collect()
        };
        assert!(
            !columns.iter().any(|column| column.contains("confidence")
                || column.contains("score")
                || column == "resolution"),
            "five axes, no score: {columns:?}"
        );
    }

    #[test]
    fn the_graph_stores_identity_and_never_source_text() {
        let fixture = Fixture::create("no-source");
        let store = fixture.store();
        let caller = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "App.run"));
        let callee = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "helper"));
        let package = GraphEndpoint::External(external());
        for endpoint in [&caller, &callee, &package] {
            store.ensure_entity(endpoint).expect("ensure");
        }
        assert!(
            store
                .insert_relation(&Relation {
                    kind: RelationKind::Calls,
                    source: caller,
                    target: callee,
                    dispatch: Dispatch::Unknown,
                    created_generation: fixture.generation(),
                })
                .expect("insert")
        );
        drop(store);

        let database = fs::read(fixture.db_path()).expect("index.db bytes");
        for body in ["return helper()", "return 41", "return 2"] {
            assert!(
                !database
                    .windows(body.len())
                    .any(|window| window == body.as_bytes()),
                "index.db must not contain source text ({body:?})"
            );
        }
    }
}
