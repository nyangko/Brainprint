//! The Workspace lookups every semantic adapter needs, stated once.
//!
//! Turning a backend location into Brainprint identity is the same
//! problem in every language: a path key has to name an ACTIVE
//! Resource, a byte span has to name exactly one Symbol or none, and a
//! site this context already answered has to be re-askable so a second
//! refresh is idempotent rather than self-withdrawing. None of that is
//! Python or TypeScript; all of it is the index.
//!
//! What stays in each backend's own adapter is everything that *is*
//! language: which request answers which gap, how a dependency file
//! gets a package identity, and what a call's dispatch means.

use brainprint_core::{ResourceId, SymbolId};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    evidence::OccurrenceRef,
    gaps::{IntendedRelation, PersistedUnresolved, UnresolvedReason},
    resource::Resource,
    symbol::OccurrenceKind,
};

/// What a byte span matched in the Symbol table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolMatch {
    One(SymbolId),
    /// Two Symbols could be meant and nothing in the answer chooses.
    /// Ordering is not evidence.
    Ambiguous,
    None,
}

/// The Symbol a byte span declares, by exact span and nothing else.
///
/// Two keys, both exact. A definition answer points at the name token,
/// which is what a `DEFINITION` Occurrence records; a type answer
/// points at the whole declaration, which is what `symbol.span` is.
/// Neither is a name lookup, because "the Symbol called `run`" is
/// exactly the ambiguity semantics is here to resolve -- and the
/// fixtures keep unrelated same-name declarations around so a test
/// fails if a name lookup ever creeps back in.
pub fn symbol_at_span(
    connection: &Connection,
    resource: ResourceId,
    start: usize,
    end: usize,
) -> Result<SymbolMatch, rusqlite::Error> {
    let start = i64::try_from(start).unwrap_or(i64::MAX);
    let end = i64::try_from(end).unwrap_or(i64::MAX);
    let uid = resource.to_bytes().to_vec();

    let by_definition: Vec<Vec<u8>> = connection
        .prepare(
            "SELECT DISTINCT symbol.uid FROM occurrence \
             JOIN resource ON resource.id = occurrence.resource_id \
             JOIN symbol ON symbol.id = occurrence.containing_symbol_id \
             WHERE resource.uid = ?1 AND occurrence.kind = 'DEFINITION' \
               AND occurrence.start_byte = ?2 AND occurrence.end_byte = ?3",
        )?
        .query_map(params![uid.clone(), start, end], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    if let Some(found) = single(&by_definition) {
        return Ok(found);
    }

    let by_span: Vec<Vec<u8>> = connection
        .prepare(
            "SELECT symbol.uid FROM symbol \
             JOIN resource ON resource.id = symbol.resource_id \
             WHERE resource.uid = ?1 AND symbol.start_byte = ?2 AND symbol.end_byte = ?3",
        )?
        .query_map(params![uid, start, end], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(single(&by_span).unwrap_or(SymbolMatch::None))
}

fn single(rows: &[Vec<u8>]) -> Option<SymbolMatch> {
    match rows.len() {
        0 => None,
        1 => rows[0]
            .as_slice()
            .try_into()
            .ok()
            .map(|bytes: [u8; 16]| SymbolMatch::One(SymbolId::from_bytes(bytes))),
        _ => Some(SymbolMatch::Ambiguous),
    }
}

/// The active Resource one Workspace-relative path key names.
///
/// Only the columns normalization needs. The full
/// [`ResourceStore`](crate::resource::ResourceStore) owns its own
/// connection, and this runs on the caller's.
pub fn resource_by_path_key(
    connection: &Connection,
    path_key: &str,
) -> Result<Option<Resource>, rusqlite::Error> {
    let row: Option<(Vec<u8>, String, String)> = connection
        .query_row(
            "SELECT uid, path_rel, resource_revision FROM resource \
             WHERE path_key = ?1 AND state = 'ACTIVE'",
            params![path_key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((uid, path_rel, revision)) = row else {
        return Ok(None);
    };
    let bytes: [u8; 16] = uid
        .as_slice()
        .try_into()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    Ok(Some(sparse(
        ResourceId::from_bytes(bytes),
        path_rel,
        path_key.to_owned(),
        revision,
    )))
}

/// The active Resource one id names.
pub fn resource_by_id(
    connection: &Connection,
    resource: ResourceId,
) -> Result<Option<Resource>, rusqlite::Error> {
    let row: Option<(String, String, String)> = connection
        .query_row(
            "SELECT path_rel, path_key, resource_revision FROM resource \
             WHERE uid = ?1 AND state = 'ACTIVE'",
            params![resource.to_bytes().to_vec()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    Ok(row.map(|(path_rel, path_key, revision)| sparse(resource, path_rel, path_key, revision)))
}

/// A Resource carrying only what normalization reads. The absent fields
/// are absent on purpose: nothing here may decide identity from a size,
/// an mtime or a fingerprint it did not load.
fn sparse(id: ResourceId, path_rel: String, path_key: String, revision: String) -> Resource {
    Resource {
        id,
        path_rel,
        path_key,
        kind: crate::resource::ResourceKind::File,
        role: crate::resource::ResourceRole::Source,
        language: None,
        size_bytes: 0,
        mtime_ns: 0,
        fingerprint: String::new(),
        content_hash: None,
        state: crate::resource::ResourceState::Active,
        resource_revision: revision,
        generated_kind: None,
        container_resource_id: None,
    }
}

/// The gaps this context is currently holding closed for `owner`.
///
/// A refresh that looked only at the open `unresolved_reference` rows
/// would find nothing the second time round -- its own merge closed
/// them -- and task 4 reads that silence as "this context withdrew its
/// contribution", so the edges it just created would be removed again.
/// Re-asking about the sites this context already answered is what
/// makes a repeated refresh idempotent instead of oscillating, and it
/// is also how a changed answer replaces an old one rather than
/// conflicting with it.
///
/// The gap is reconstructed from what task 4 displaced, so nothing new
/// is stored to make this possible.
pub fn displaced_gaps(
    connection: &Connection,
    context_key: &str,
    owner: ResourceId,
) -> Result<Vec<PersistedUnresolved>, rusqlite::Error> {
    let mut statement = connection.prepare(
        "SELECT occurrence.kind, occurrence.start_byte, occurrence.end_byte, \
                semantic_evidence.displaced_intended_kind, \
                semantic_evidence.displaced_lookup_name, \
                semantic_evidence.displaced_module_hint, \
                semantic_evidence.displaced_reason \
         FROM semantic_evidence \
         JOIN occurrence ON occurrence.id = semantic_evidence.occurrence_id \
         JOIN resource ON resource.id = occurrence.resource_id \
         WHERE semantic_evidence.context_key = ?1 AND resource.uid = ?2 \
           AND semantic_evidence.displaced_intended_kind IS NOT NULL \
         ORDER BY occurrence.start_byte, occurrence.end_byte, occurrence.kind",
    )?;
    type Row = (
        String,
        i64,
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = statement
        .query_map(params![context_key, owner.to_bytes().to_vec()], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })?
        .collect::<Result<_, _>>()?;

    Ok(rows
        .into_iter()
        .filter_map(|raw| {
            Some(PersistedUnresolved {
                occurrence: OccurrenceRef {
                    kind: OccurrenceKind::parse_public(&raw.0).ok()?,
                    start_byte: usize::try_from(raw.1).unwrap_or(0),
                    end_byte: usize::try_from(raw.2).unwrap_or(0),
                },
                intended: IntendedRelation::parse(&raw.3).ok()?,
                lookup_name: raw.4.unwrap_or_default(),
                module_hint: raw.5,
                reason: raw
                    .6
                    .and_then(|reason| UnresolvedReason::parse(&reason).ok())
                    .unwrap_or(UnresolvedReason::NoStructuralBinding),
                candidate_truncated: false,
                // The candidate list is the structural resolver's
                // working set and task 4 does not keep it. Re-asking
                // needs the site and the intended kind, not the guesses.
                candidates: Vec::new(),
                resolution_context_key: None,
            })
        })
        .collect())
}
