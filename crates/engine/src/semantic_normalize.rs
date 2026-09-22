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
    graph::{GraphEndpoint, RelationKind},
    resource::Resource,
    symbol::{Occurrence, OccurrenceKind},
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

// ---------------------------------------------------------------------
// The sites one Resource offers
// ---------------------------------------------------------------------

/// The relation kinds an adapter can resolve from a source occurrence.
///
/// Each one is a site I3 already recorded, so the answer has somewhere
/// exact to anchor. `OVERRIDES` is absent because it is not written at
/// a site at all -- it is derived from proven inheritance (see
/// [`crate::semantic_overrides`]).
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
    #[must_use]
    pub const fn of_relation(kind: RelationKind) -> Option<Self> {
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

    #[must_use]
    pub const fn of_intended(intended: IntendedRelation) -> Option<Self> {
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
    #[must_use]
    pub const fn of_occurrence(kind: OccurrenceKind) -> Option<Self> {
        match kind {
            OccurrenceKind::ImportSite => Some(Self::Imports),
            OccurrenceKind::CallSite => Some(Self::Calls),
            OccurrenceKind::ReferenceSite => Some(Self::References),
            OccurrenceKind::TypeSite | OccurrenceKind::Definition | OccurrenceKind::KeySite => None,
        }
    }

    /// The canonical relation this site becomes once it resolves.
    #[must_use]
    pub const fn relation_kind(self) -> RelationKind {
        match self {
            Self::Imports => RelationKind::Imports,
            Self::Calls => RelationKind::Calls,
            Self::References => RelationKind::References,
            Self::UsesType => RelationKind::UsesType,
            Self::Extends => RelationKind::Extends,
            Self::Implements => RelationKind::Implements,
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

/// The endpoint a site is *from*: the Symbol that lexically contains
/// it, or the Resource at file level. The same rule I3 uses, so
/// structural and semantic evidence for one edge agree on its source.
#[must_use]
pub fn source_endpoint(
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
