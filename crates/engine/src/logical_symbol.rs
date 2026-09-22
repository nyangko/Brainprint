//! One semantic symbol, several source declarations.
//!
//! Most languages declare a thing once, and [`Symbol`](crate::symbol::Symbol)
//! -- one Resource, one span, one current declaration -- is exactly
//! right for them. Some languages do not. A C# `partial class Runner`
//! split across two files is **one type**, and the compiler answers a
//! reference to it with *both* declarations.
//!
//! #19 task 12 locked what that must not become:
//!
//! * not one Occurrence carrying two bindings -- task 4's
//!   `UNIQUE (context_key, occurrence_id)` stays, because a reference
//!   really does bind to one thing;
//! * not "whichever declaration the backend listed first", which is a
//!   coin toss dressed as an answer;
//! * not a synthetic Symbol with borrowed coordinates, and not
//!   `ExternalEntity` or `DomainEntity` pressed into service.
//!
//! What was missing was never a second binding. It was the *thing* being
//! bound to. This module is that thing:
//!
//! ```text
//! Runner.Part1.cs   Symbol A ─┐
//!                             ├─ LogicalSymbol "Core.Runner"
//! Runner.Part2.cs   Symbol B ─┘
//!
//! a reference ──(one binding)──> LogicalSymbol
//! ```
//!
//! Nothing here is C#. The identity is built from meaning a compiler can
//! state -- a semantic world, a qualified name, a kind, a generic arity
//! -- so a later language that merges declarations uses the same table
//! without renaming anything. Languages whose declarations are singular
//! never create one, which is why Python, TypeScript and Svelte are
//! untouched by this file existing.
//!
//! ## What identity is *not* built from
//!
//! Declaration file paths. Adding or removing one part of a partial type
//! must leave the type's identity alone while another part still
//! declares it, so the sorted-paths key that suggests itself first is
//! exactly wrong. Nor a backend key: no Roslyn `SymbolKey`, `ProjectId`
//! or `DocumentId` reaches storage. A backend may *prove* the grouping;
//! it does not get to name it.

use std::fmt;

use brainprint_core::{LogicalSymbolId, SymbolId};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{db, symbol::SymbolKind};

/// What a logical symbol is made of, before it has an identity.
///
/// Every field is something a compiler can state about meaning, and
/// nothing in it moves when a declaration is added or removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalIdentity {
    /// The semantic world this symbol lives in: the
    /// [`AnalysisContext`](crate::semantic::AnalysisContext) key. It
    /// already carries the Workspace, so two worktrees never share a
    /// logical symbol, and it carries the analysis toolchain, so two
    /// incompatible semantic worlds never collapse into one.
    pub context_key: String,
    /// The compilation unit inside that world -- a `.csproj`, by its
    /// Workspace-relative path key. Two projects that both declare
    /// `Core.Runner` declare two different types, and this is what keeps
    /// them apart.
    ///
    /// A locator by shape, identity by use: it is a path *key*, never a
    /// machine path, and it is exactly as stable as the project file.
    pub project_key: String,
    /// The fully qualified name, as the structural tier already computes
    /// it.
    pub qualified_name: String,
    pub kind: SymbolKind,
    /// How many type parameters the declaration writes. `Box<T>` and
    /// `Box` are different types and must not join.
    pub arity: usize,
    /// Anything else that makes two otherwise identical declarations
    /// belong to different semantic worlds -- a target framework, a
    /// build configuration.
    ///
    /// Empty today. #19 task 12 measured that the official Roslyn LSP
    /// does not say *which* target framework answered for a
    /// multi-targeted project, so there is nothing truthful to put here
    /// yet; the field exists so that adding it later is a value change
    /// rather than an identity migration.
    pub discriminator: String,
}

impl LogicalIdentity {
    /// The deterministic fingerprint this identity is stored under.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let arity = self.arity.to_string();
        db::fingerprint(
            "logical-symbol-1",
            &[
                ("context", &self.context_key),
                ("project", &self.project_key),
                ("qualified_name", &self.qualified_name),
                ("kind", self.kind.as_str()),
                ("arity", &arity),
                ("discriminator", &self.discriminator),
            ],
        )
    }

    /// What a reader sees. Diagnostic only -- never identity.
    #[must_use]
    pub fn display_name(&self) -> String {
        if self.arity == 0 {
            self.qualified_name.clone()
        } else {
            format!("{}`{}", self.qualified_name, self.arity)
        }
    }
}

/// One stored logical symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalSymbol {
    pub id: LogicalSymbolId,
    pub context_key: String,
    pub kind: SymbolKind,
    pub display_name: String,
}

/// Why a logical symbol could not be read or written.
#[derive(Debug)]
pub enum LogicalSymbolError {
    Sqlite(rusqlite::Error),
    /// A stored row carries a kind this build does not know.
    UnknownKind {
        raw: String,
    },
}

impl fmt::Display for LogicalSymbolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "logical symbol: {error}"),
            Self::UnknownKind { raw } => write!(formatter, "unknown symbol kind {raw:?}"),
        }
    }
}

impl std::error::Error for LogicalSymbolError {}

impl From<rusqlite::Error> for LogicalSymbolError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// Find or create the logical symbol one identity names.
///
/// Keyed on the fingerprint, so the same meaning resolves to the same
/// identity however many times it is proved, and from whichever
/// declaration proved it first.
///
/// # Errors
/// When the index cannot be written.
pub fn ensure(
    connection: &Connection,
    identity: &LogicalIdentity,
    created_generation: i64,
) -> Result<LogicalSymbolId, LogicalSymbolError> {
    let fingerprint = identity.fingerprint();
    if let Some(existing) = by_fingerprint(connection, &fingerprint)? {
        return Ok(existing);
    }
    let id = LogicalSymbolId::generate();
    connection.execute(
        "INSERT INTO logical_symbol \
         (uid, identity_fingerprint, context_key, kind, display_name, created_generation) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT (identity_fingerprint) DO NOTHING",
        params![
            id.to_bytes().to_vec(),
            fingerprint,
            identity.context_key,
            identity.kind.as_str(),
            identity.display_name(),
            created_generation,
        ],
    )?;
    // A concurrent writer may have won the insert; the fingerprint is the
    // identity, so read back rather than assume.
    by_fingerprint(connection, &fingerprint)?.map_or(Ok(id), Ok)
}

fn by_fingerprint(
    connection: &Connection,
    fingerprint: &str,
) -> Result<Option<LogicalSymbolId>, LogicalSymbolError> {
    let uid: Option<Vec<u8>> = connection
        .query_row(
            "SELECT uid FROM logical_symbol WHERE identity_fingerprint = ?1",
            params![fingerprint],
            |row| row.get(0),
        )
        .optional()?;
    Ok(uid
        .and_then(|bytes| <[u8; 16]>::try_from(bytes.as_slice()).ok())
        .map(LogicalSymbolId::from_bytes))
}

/// Read one logical symbol.
///
/// # Errors
/// When the index cannot be read.
pub fn read(
    connection: &Connection,
    id: LogicalSymbolId,
) -> Result<Option<LogicalSymbol>, LogicalSymbolError> {
    let row: Option<(String, String, String)> = connection
        .query_row(
            "SELECT context_key, kind, display_name FROM logical_symbol WHERE uid = ?1",
            params![id.to_bytes().to_vec()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((context_key, kind, display_name)) = row else {
        return Ok(None);
    };
    Ok(Some(LogicalSymbol {
        id,
        context_key,
        kind: SymbolKind::parse_public(&kind)
            .map_err(|_| LogicalSymbolError::UnknownKind { raw: kind })?,
        display_name,
    }))
}

/// Record that `declaration` is one of `logical`'s declarations.
///
/// Replaces the membership row rather than adding a second, so proving
/// the same declaration twice is idempotent.
///
/// # Errors
/// When the index cannot be written.
pub fn declare(
    connection: &Connection,
    logical: LogicalSymbolId,
    declaration: SymbolId,
    context_key: &str,
    generation_id: i64,
) -> Result<bool, LogicalSymbolError> {
    let changed = connection.execute(
        "INSERT INTO logical_symbol_declaration \
             (logical_symbol_id, symbol_id, context_key, generation_id) \
         SELECT logical_symbol.id, symbol.id, ?3, ?4 \
         FROM logical_symbol, symbol \
         WHERE logical_symbol.uid = ?1 AND symbol.uid = ?2 \
         ON CONFLICT (logical_symbol_id, symbol_id) \
         DO UPDATE SET generation_id = excluded.generation_id, \
                       context_key = excluded.context_key",
        params![
            logical.to_bytes().to_vec(),
            declaration.to_bytes().to_vec(),
            context_key,
            generation_id,
        ],
    )?;
    Ok(changed > 0)
}

/// Every current declaration of one logical symbol, in a stable order.
///
/// This is the projection every Agent-facing query goes through: a
/// caller never has to understand the grouping, it asks for the
/// declarations and gets ordinary source Symbols with ordinary spans.
///
/// # Errors
/// When the index cannot be read.
pub fn declarations(
    connection: &Connection,
    logical: LogicalSymbolId,
) -> Result<Vec<SymbolId>, LogicalSymbolError> {
    let mut statement = connection.prepare(
        "SELECT symbol.uid FROM logical_symbol_declaration \
         JOIN logical_symbol ON logical_symbol.id = logical_symbol_declaration.logical_symbol_id \
         JOIN symbol ON symbol.id = logical_symbol_declaration.symbol_id \
         JOIN resource ON resource.id = symbol.resource_id \
         WHERE logical_symbol.uid = ?1 \
         ORDER BY resource.path_key, symbol.start_byte",
    )?;
    let rows: Vec<Vec<u8>> = statement
        .query_map(params![logical.to_bytes().to_vec()], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|bytes| <[u8; 16]>::try_from(bytes.as_slice()).ok())
        .map(SymbolId::from_bytes)
        .collect())
}

/// The logical symbols one declaration belongs to.
///
/// Normally none or one. Reading it back is how a lifecycle decides
/// whether replacing a Resource's structure orphaned a group.
///
/// # Errors
/// When the index cannot be read.
pub fn groups_of(
    connection: &Connection,
    declaration: SymbolId,
) -> Result<Vec<LogicalSymbolId>, LogicalSymbolError> {
    let mut statement = connection.prepare(
        "SELECT logical_symbol.uid FROM logical_symbol_declaration \
         JOIN logical_symbol ON logical_symbol.id = logical_symbol_declaration.logical_symbol_id \
         JOIN symbol ON symbol.id = logical_symbol_declaration.symbol_id \
         WHERE symbol.uid = ?1 ORDER BY logical_symbol.identity_fingerprint",
    )?;
    let rows: Vec<Vec<u8>> = statement
        .query_map(params![declaration.to_bytes().to_vec()], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|bytes| <[u8; 16]>::try_from(bytes.as_slice()).ok())
        .map(LogicalSymbolId::from_bytes)
        .collect())
}

/// Drop every logical symbol in `context_key` that no longer has a
/// declaration.
///
/// Membership follows its Symbols by `ON DELETE CASCADE`, which is safe
/// here in a way it deliberately is not for `semantic_evidence`: a
/// membership row is *derived*, so losing it costs nothing that has to
/// be restored, whereas a withdrawn semantic proof owes an honest gap
/// back. What the cascade cannot do is notice that the last declaration
/// went, and a group with no declarations is not a symbol -- so this
/// removes it, and with it the `graph_entity` that pointed at it.
///
/// Returns how many groups were removed.
///
/// # Errors
/// When the index cannot be written.
pub fn collect_orphans(
    connection: &Connection,
    context_key: &str,
) -> Result<usize, LogicalSymbolError> {
    // The entity row first: it references the logical symbol, and
    // nothing references the entity once its relations are gone.
    connection.execute(
        "DELETE FROM graph_entity WHERE logical_symbol_id IN ( \
             SELECT logical_symbol.id FROM logical_symbol \
             WHERE logical_symbol.context_key = ?1 \
               AND NOT EXISTS ( \
                   SELECT 1 FROM logical_symbol_declaration \
                   WHERE logical_symbol_declaration.logical_symbol_id = logical_symbol.id) \
               AND NOT EXISTS ( \
                   SELECT 1 FROM relation \
                   JOIN graph_entity AS held \
                     ON held.id IN (relation.source_entity_id, relation.target_entity_id) \
                   WHERE held.logical_symbol_id = logical_symbol.id))",
        params![context_key],
    )?;
    let removed = connection.execute(
        "DELETE FROM logical_symbol \
         WHERE context_key = ?1 \
           AND NOT EXISTS ( \
               SELECT 1 FROM logical_symbol_declaration \
               WHERE logical_symbol_declaration.logical_symbol_id = logical_symbol.id) \
           AND NOT EXISTS ( \
               SELECT 1 FROM graph_entity \
               WHERE graph_entity.logical_symbol_id = logical_symbol.id)",
        params![context_key],
    )?;
    Ok(removed)
}

/// Forget every membership this context recorded for one declaration.
///
/// What a withdrawal calls before the structural replacement that
/// removes the Symbol, so a stale part cannot outlive the proof that put
/// it in the group.
///
/// # Errors
/// When the index cannot be written.
pub fn undeclare_resource(
    connection: &Connection,
    context_key: &str,
    resource: brainprint_core::ResourceId,
) -> Result<usize, LogicalSymbolError> {
    let removed = connection.execute(
        "DELETE FROM logical_symbol_declaration \
         WHERE context_key = ?1 AND symbol_id IN ( \
             SELECT symbol.id FROM symbol \
             JOIN resource ON resource.id = symbol.resource_id \
             WHERE resource.uid = ?2)",
        params![context_key, resource.to_bytes().to_vec()],
    )?;
    Ok(removed)
}
