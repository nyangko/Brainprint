//! Direct forward/reverse relation queries (#17 task 8).
//!
//! Tasks 4-6 resolved edges and task 7 persisted the gaps. This is the
//! read side: the typed questions an Agent actually asks -- who calls
//! this, what does this call, who imports this file, what references
//! this symbol, what extends this class -- answered from `index.db`
//! alone, so that the answer does not send the caller back to `rg`.
//!
//! ## Direction is a query, not an edge
//!
//! `CALLS A -> B` is one row. [`RelationIndex::callees`] reads it from
//! the source side and [`RelationIndex::callers`] from the target side;
//! both hand back the same canonical relation with the direction they
//! were asked in. No `CALLED_BY` row exists, and none is created --
//! the reverse lookup is the existing target index over the same row.
//!
//! ## What a result is allowed to claim
//!
//! A [`RelationResult`] is always [`Resolution::Resolved`]: a stored
//! relation is a confirmed edge. Everything the structural tier could
//! not confirm comes back separately as a [`RelationGap`], never merged
//! into the confirmed list and never promoted by name similarity.
//!
//! This is what keeps `confirmed_count() == 0` honest.
//! [`Coverage::is_complete`] is the difference between "nothing calls
//! this" and "nothing *confirmed* calls this, and here are four
//! unresolved call sites that might". A reverse question is weaker
//! still: an unresolved site names no target, so only a gap that
//! persisted this target among its candidates can be attributed to it,
//! and the rest are counted rather than attached
//! ([`GapAttribution::TargetCandidateOnly`]).
//!
//! ## Identity and evidence
//!
//! Results carry [`GraphEndpoint`]s -- Resource, Symbol, External,
//! Domain -- and never a `graph_entity.id`, `relation.id`, or
//! `occurrence.id`. Every canonical relation carries *all* its evidence
//! spans: two call sites in one file are one relation with two
//! [`EvidenceLocation`]s, not two relations. A location is a locator
//! (resource, byte range, line/column), never source text -- reading
//! the current source at that range is task 9.
//!
//! [`Support`] and [`Freshness`] are derived through task 2's pure
//! functions from the structural state and revisions that already
//! exist. Nothing here stores a second copy of either.
//!
//! Out of scope: source ranges and snippets (task 9), traversal and
//! impact (task 10), related tests (task 11), env/config relations
//! (task 12), lifecycle wiring (task 13), and semantic resolution (I4).

use std::{collections::HashMap, error::Error, fmt, path::Path};

use brainprint_core::{ResourceId, SymbolId};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    coverage::{AnswerState, CoverageLimit, CoverageReport},
    db::DbOpenError,
    gaps::{GapError, IntendedRelation, UnresolvedReason},
    graph::{self, GraphEndpoint, GraphError, Relation, RelationKind},
    graph_lifecycle,
    merge::{self, MergeError, SemanticScope},
    parser::{SourcePoint, SourceSpan},
    resolution::{
        Dispatch, Freshness, Resolution, Support, TargetScope, freshness_of, support_of,
        weaker_freshness, weaker_support,
    },
    schema, structural,
    symbol::{OccurrenceKind, SymbolError},
};

/// The kinds a type/inheritance query covers.
pub const TYPE_KINDS: [RelationKind; 3] = [
    RelationKind::Extends,
    RelationKind::Implements,
    RelationKind::UsesType,
];

/// Which way the caller asked. A property of the question, not of the
/// stored edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Edges leaving the queried endpoint.
    Outgoing,
    /// Edges arriving at it, read through the same rows.
    Incoming,
}

/// One exact source location proving a relation, or stating a gap.
///
/// A locator only: byte range, line/column, and the identity of what
/// contains it. No source text (task 9 reads the range).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceLocation {
    /// The Resource that owns -- and may replace -- this evidence.
    pub resource: ResourceId,
    /// The smallest Symbol lexically containing the span, or `None` at
    /// file level.
    pub containing_symbol: Option<SymbolId>,
    pub occurrence_kind: OccurrenceKind,
    pub span: SourceSpan,
    /// The owner revision the evidence was extracted from.
    pub basis_revision: String,
    /// How completely the owner is structurally covered.
    pub support: Support,
    /// Whether the evidence still describes the owner as it is now.
    pub freshness: Freshness,
}

/// One confirmed canonical relation, as an answer to a direct query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationResult {
    pub kind: RelationKind,
    pub source: GraphEndpoint,
    pub target: GraphEndpoint,
    /// The direction the caller asked in. The row is the same either
    /// way; this says which end was the anchor.
    pub direction: Direction,
    pub dispatch: Dispatch,
    pub target_scope: TargetScope,
    /// Always [`Resolution::Resolved`] -- a relation row is confirmed.
    pub resolution: Resolution,
    /// The weakest support across this relation's evidence.
    pub support: Support,
    /// The weakest freshness across this relation's evidence.
    pub freshness: Freshness,
    /// Every evidence span for this canonical edge, in deterministic
    /// order. Several sites do not make several relations.
    pub evidence: Vec<EvidenceLocation>,
}

/// One use site the structural tier could not confirm a target for.
///
/// Never a relation: it has no confirmed target, and the candidates it
/// carries stay candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationGap {
    pub location: EvidenceLocation,
    /// What the relation would have been, had the target been known.
    pub intended: IntendedRelation,
    /// The name or specifier as written.
    pub lookup_name: String,
    pub module_hint: Option<String>,
    pub reason: UnresolvedReason,
    /// [`Resolution::Candidate`] when the resolver had canonical
    /// candidates in hand, [`Resolution::Unresolved`] otherwise. One
    /// candidate is still a candidate.
    pub resolution: Resolution,
    /// Canonical candidate identities, in stored order. Never ranked,
    /// never promoted.
    pub candidates: Vec<GraphEndpoint>,
    /// Whether the candidate list was cut (#17 task 7).
    pub candidate_truncated: bool,
    pub resolution_context_key: Option<String>,
}

/// How far a gap list can be trusted to be the whole story.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapAttribution {
    /// A forward query: every gap the queried source owns is listed, so
    /// an empty gap list means the known structural evidence in scope is
    /// fully accounted for.
    SourceScoped,
    /// A reverse query: an unresolved site states no target, so only a
    /// gap that persisted this target among its candidates can be
    /// attributed to it. Others are counted
    /// ([`Coverage::unattributed`]), never attached by name.
    TargetCandidateOnly,
}

/// The Resource a forward query was anchored in, and its state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeState {
    pub resource: ResourceId,
    pub support: Support,
    pub freshness: Freshness,
}

/// What the confirmed results do *not* say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coverage {
    pub attribution: GapAttribution,
    /// Gaps returned with this answer.
    pub gaps: usize,
    /// Of those, ones with canonical candidates.
    pub ambiguous: usize,
    /// Of those, ones a semantic backend (I4) would have to answer.
    pub requires_semantics: usize,
    /// Of those, ones whose construct this tier does not model.
    pub unsupported_construct: usize,
    /// Of those, ones whose candidate list was cut.
    pub truncated: usize,
    /// Unresolved sites of a matching intended kind that exist but
    /// cannot be attributed to this target. Always zero for a forward
    /// query.
    pub unattributed: usize,
    /// The queried source's own Resource state, when the query had one
    /// (a forward query from a Resource or Symbol).
    pub scope: Option<ScopeState>,
    /// What the semantic tier contributes to this scope, and what it
    /// cannot currently say (#19 task 4). Empty when nothing semantic
    /// touches the scope, which is the whole of I3.
    pub semantic: SemanticScope,
}

impl Coverage {
    /// Every specific reason this answer is not the whole story
    /// (#17 task 14).
    ///
    /// The one place a direct query's completeness is decided, so that
    /// impact, related tests and prepared inspection do not each grow
    /// a slightly different rule.
    #[must_use]
    pub fn limits(&self) -> CoverageReport {
        let mut report = CoverageReport::new();
        report.note_if(
            !matches!(self.attribution, GapAttribution::SourceScoped),
            CoverageLimit::ReverseScopeNotEnumerable,
        );
        report.note_if(self.gaps > 0, CoverageLimit::UnresolvedEvidence);
        report.note_if(self.ambiguous > 0, CoverageLimit::AmbiguousCandidates);
        report.note_if(
            self.requires_semantics > 0,
            CoverageLimit::RequiresSemantics,
        );
        report.note_if(
            self.unsupported_construct > 0,
            CoverageLimit::UnsupportedConstruct,
        );
        report.note_if(self.truncated > 0, CoverageLimit::CandidateTruncated);
        report.note_if(self.unattributed > 0, CoverageLimit::UnattributedGaps);
        // Two tiers confirming different targets is not a candidate set
        // and not a stale index: it is its own reason a scope is not a
        // clean answer.
        report.note_if(self.semantic.conflicts > 0, CoverageLimit::SemanticConflict);
        report.note_if(self.semantic.not_current, CoverageLimit::SemanticNotCurrent);
        match self.scope {
            Some(scope) => {
                report.note_support(scope.support);
                report.note_freshness(scope.freshness);
            }
            // A forward query with no indexed Resource behind it --
            // an external package, a domain key -- states nothing
            // about its own outgoing edges. A reverse query has no
            // scope by construction and is already limited above.
            None => report.note_if(
                matches!(self.attribution, GapAttribution::SourceScoped),
                CoverageLimit::UnsupportedScope,
            ),
        }
        report
    }

    /// Whether the confirmed results are everything the known
    /// structural evidence supports -- and therefore whether zero may
    /// be read as "none".
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.limits().is_complete()
    }
}

/// One direct query's whole answer: what is confirmed, and what is not
/// known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationAnswer {
    pub direction: Direction,
    /// The kinds asked for. Empty means every kind.
    pub kinds: Vec<RelationKind>,
    /// Confirmed relations, deduplicated by canonical identity and in
    /// deterministic order.
    pub confirmed: Vec<RelationResult>,
    pub gaps: Vec<RelationGap>,
    pub coverage: Coverage,
}

impl RelationAnswer {
    /// How many confirmed relations this scope has.
    ///
    /// Known, because the scope is returned whole: this query surface
    /// has no cap to hide behind.
    #[must_use]
    pub fn confirmed_count(&self) -> usize {
        self.confirmed.len()
    }

    /// Whether an empty confirmed list may be reported as "there are
    /// none".
    #[must_use]
    pub fn confirmed_zero_is_none(&self) -> bool {
        matches!(self.answer_state(), AnswerState::NoneUnderCompleteCoverage)
    }

    /// What this answer is allowed to claim (#17 task 14).
    #[must_use]
    pub fn answer_state(&self) -> AnswerState {
        self.coverage.limits().state(self.confirmed_count())
    }
}

/// Failure answering a relation query.
#[derive(Debug)]
pub enum RelationError {
    Open(DbOpenError),
    Sqlite(rusqlite::Error),
    Graph(GraphError),
    Gap(GapError),
    Symbol(SymbolError),
    Structural {
        detail: String,
    },
    /// A type/inheritance query was narrowed to a kind that is not one.
    NotATypeKind {
        kind: RelationKind,
    },
    /// Reading the scope's semantic contribution failed (#19 task 4).
    Semantic(MergeError),
}

impl fmt::Display for RelationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(error) => write!(formatter, "open index.db: {error}"),
            Self::Sqlite(error) => write!(formatter, "index.db: {error}"),
            Self::Graph(error) => write!(formatter, "graph: {error}"),
            Self::Semantic(error) => write!(formatter, "semantic: {error}"),
            Self::Gap(error) => write!(formatter, "gap: {error}"),
            Self::Symbol(error) => write!(formatter, "symbol: {error}"),
            Self::Structural { detail } => write!(formatter, "structural state: {detail}"),
            Self::NotATypeKind { kind } => {
                write!(formatter, "{kind} is not a type or inheritance relation")
            }
        }
    }
}

impl Error for RelationError {}

impl From<DbOpenError> for RelationError {
    fn from(error: DbOpenError) -> Self {
        Self::Open(error)
    }
}

impl From<rusqlite::Error> for RelationError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<GraphError> for RelationError {
    fn from(error: GraphError) -> Self {
        Self::Graph(error)
    }
}

impl From<GapError> for RelationError {
    fn from(error: GapError) -> Self {
        Self::Gap(error)
    }
}

impl From<SymbolError> for RelationError {
    fn from(error: SymbolError) -> Self {
        Self::Symbol(error)
    }
}

/// Direct relation queries over one `index.db`.
pub struct RelationIndex {
    connection: Connection,
}

impl RelationIndex {
    /// Open (creating/migrating if needed) the `index.db` at `path`.
    pub fn open(path: &Path) -> Result<Self, RelationError> {
        Ok(Self::from_connection(schema::index::open(path)?.connection))
    }

    /// Wrap an already-opened `index.db` connection.
    #[must_use]
    pub fn from_connection(connection: Connection) -> Self {
        Self { connection }
    }

    /// This index's connection.
    #[must_use]
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Relations leaving `source`, narrowed to `kinds` (empty: all).
    pub fn outgoing(
        &self,
        source: &GraphEndpoint,
        kinds: &[RelationKind],
    ) -> Result<RelationAnswer, RelationError> {
        self.answer(source, Direction::Outgoing, kinds)
    }

    /// Relations arriving at `target`, narrowed to `kinds` (empty: all).
    ///
    /// The same rows as [`Self::outgoing`] would return from the other
    /// end. Nothing reverse is stored to make this work.
    pub fn incoming(
        &self,
        target: &GraphEndpoint,
        kinds: &[RelationKind],
    ) -> Result<RelationAnswer, RelationError> {
        self.answer(target, Direction::Incoming, kinds)
    }

    /// What `source` calls: confirmed outgoing CALLS.
    pub fn callees(&self, source: &GraphEndpoint) -> Result<RelationAnswer, RelationError> {
        self.outgoing(source, &[RelationKind::Calls])
    }

    /// What calls `target`: confirmed incoming CALLS.
    pub fn callers(&self, target: &GraphEndpoint) -> Result<RelationAnswer, RelationError> {
        self.incoming(target, &[RelationKind::Calls])
    }

    /// What `source` imports: confirmed outgoing IMPORTS.
    pub fn imports(&self, source: ResourceId) -> Result<RelationAnswer, RelationError> {
        self.outgoing(&GraphEndpoint::Resource(source), &[RelationKind::Imports])
    }

    /// What imports `target`: confirmed incoming IMPORTS, read through
    /// the same edge rather than an `IMPORTED_BY` relation.
    pub fn importers(&self, target: &GraphEndpoint) -> Result<RelationAnswer, RelationError> {
        self.incoming(target, &[RelationKind::Imports])
    }

    /// What references `target` as a value: confirmed incoming
    /// REFERENCES.
    pub fn references(&self, target: &GraphEndpoint) -> Result<RelationAnswer, RelationError> {
        self.incoming(target, &[RelationKind::References])
    }

    /// Direct type and inheritance relations: EXTENDS, IMPLEMENTS,
    /// USES_TYPE, or whichever subset `kinds` asks for (empty: all
    /// three). One hop only -- a hierarchy walk is task 10.
    pub fn type_relations(
        &self,
        endpoint: &GraphEndpoint,
        direction: Direction,
        kinds: &[RelationKind],
    ) -> Result<RelationAnswer, RelationError> {
        for kind in kinds {
            if !TYPE_KINDS.contains(kind) {
                return Err(RelationError::NotATypeKind { kind: *kind });
            }
        }
        let requested = if kinds.is_empty() {
            &TYPE_KINDS[..]
        } else {
            kinds
        };
        self.answer(endpoint, direction, requested)
    }

    fn answer(
        &self,
        endpoint: &GraphEndpoint,
        direction: Direction,
        kinds: &[RelationKind],
    ) -> Result<RelationAnswer, RelationError> {
        let mut states = StateCache::default();
        let mut relations = Vec::new();
        if kinds.is_empty() {
            relations.extend(self.relations(endpoint, direction, None)?);
        } else {
            for kind in kinds {
                relations.extend(self.relations(endpoint, direction, Some(*kind))?);
            }
        }

        let mut confirmed = Vec::new();
        for relation in relations {
            let evidence = self.evidence_of(&relation, &mut states)?;
            confirmed.push(RelationResult {
                kind: relation.kind,
                direction,
                dispatch: relation.dispatch,
                target_scope: relation.target_scope(),
                resolution: relation.resolution(),
                support: evidence
                    .iter()
                    .map(|location| location.support)
                    .fold(Support::Supported, weaker_support),
                freshness: evidence
                    .iter()
                    .map(|location| location.freshness)
                    .fold(Freshness::Fresh, weaker_freshness),
                evidence,
                source: relation.source,
                target: relation.target,
            });
        }
        // Identity order, never row order: the same graph answers in the
        // same sequence in any index.db.
        confirmed.sort_by(|left, right| result_order(left).cmp(&result_order(right)));
        confirmed.dedup_by(|left, right| {
            left.kind == right.kind && left.source == right.source && left.target == right.target
        });

        let (gaps, attribution, unattributed) = match direction {
            Direction::Outgoing => (
                self.source_gaps(endpoint, kinds, &mut states)?,
                GapAttribution::SourceScoped,
                0,
            ),
            Direction::Incoming => {
                let gaps = self.candidate_gaps(endpoint, kinds, &mut states)?;
                let unattributed = self.unattributed_gaps(kinds)?.saturating_sub(gaps.len());
                (gaps, GapAttribution::TargetCandidateOnly, unattributed)
            }
        };

        let scope = match (direction, endpoint) {
            (Direction::Outgoing, GraphEndpoint::Resource(id)) => {
                Some(self.scope_state(*id, &mut states)?)
            }
            (Direction::Outgoing, GraphEndpoint::Symbol(id)) => match self.symbol_anchor(*id)? {
                Some((_, resource)) => Some(self.scope_state(resource, &mut states)?),
                None => None,
            },
            _ => None,
        };

        let semantic = match scope {
            Some(state) => merge::semantic_scope(&self.connection, state.resource)
                .map_err(RelationError::Semantic)?,
            // A scope with no indexed Resource behind it has no semantic
            // contribution to describe either.
            None => SemanticScope::default(),
        };

        let coverage = Coverage {
            attribution,
            gaps: gaps.len(),
            ambiguous: gaps.iter().filter(|gap| !gap.candidates.is_empty()).count(),
            requires_semantics: gaps
                .iter()
                .filter(|gap| gap.reason.requires_semantics())
                .count(),
            unsupported_construct: gaps
                .iter()
                .filter(|gap| gap.reason.is_unsupported_construct())
                .count(),
            truncated: gaps.iter().filter(|gap| gap.candidate_truncated).count(),
            unattributed,
            scope,
            semantic,
        };

        Ok(RelationAnswer {
            direction,
            kinds: kinds.to_vec(),
            confirmed,
            gaps,
            coverage,
        })
    }

    fn relations(
        &self,
        endpoint: &GraphEndpoint,
        direction: Direction,
        kind: Option<RelationKind>,
    ) -> Result<Vec<Relation>, RelationError> {
        Ok(match direction {
            Direction::Outgoing => graph::relations_from(&self.connection, endpoint, kind)?,
            Direction::Incoming => graph::relations_to(&self.connection, endpoint, kind)?,
        })
    }

    /// Every exact Occurrence proving one canonical relation.
    fn evidence_of(
        &self,
        relation: &Relation,
        states: &mut StateCache,
    ) -> Result<Vec<EvidenceLocation>, RelationError> {
        let Some(relation_id) = graph::relation_row_id(&self.connection, &relation.key())? else {
            return Ok(Vec::new());
        };
        let mut statement = self.connection.prepare(
            "SELECT resource.uid, resource.resource_revision, symbol.uid, occurrence.kind, \
                    occurrence.start_byte, occurrence.end_byte, occurrence.start_line, \
                    occurrence.start_col, occurrence.end_line, occurrence.end_col, \
                    occurrence.resource_revision \
             FROM occurrence \
             JOIN resource ON resource.id = occurrence.resource_id \
             LEFT JOIN symbol ON symbol.id = occurrence.containing_symbol_id \
             WHERE occurrence.relation_id = ?1 AND resource.state = 'ACTIVE' \
             ORDER BY resource.path_key, occurrence.start_byte, occurrence.end_byte, \
                      occurrence.kind",
        )?;
        let rows = statement.query_map(params![relation_id], raw_location_row)?;
        let mut found = Vec::new();
        for row in rows {
            found.push(self.decode_location(row?, states)?);
        }
        Ok(found)
    }

    /// The gaps the queried source itself owns.
    ///
    /// Scoped exactly the way a confirmed edge's source is decided
    /// (#17 task 5): a Symbol owns the use sites it lexically contains,
    /// and a Resource owns the file-level ones.
    fn source_gaps(
        &self,
        endpoint: &GraphEndpoint,
        kinds: &[RelationKind],
        states: &mut StateCache,
    ) -> Result<Vec<RelationGap>, RelationError> {
        match endpoint {
            GraphEndpoint::Resource(id) => self.gaps_where(
                "resource.uid = ?1 AND occurrence.containing_symbol_id IS NULL",
                &id.to_bytes().to_vec(),
                kinds,
                states,
            ),
            GraphEndpoint::Symbol(id) => match self.symbol_anchor(*id)? {
                Some((row_id, _)) => self.gaps_where(
                    "occurrence.containing_symbol_id = ?1",
                    &row_id,
                    kinds,
                    states,
                ),
                None => Ok(Vec::new()),
            },
            // An external package or domain entity owns no source, so it
            // states no use sites of its own.
            GraphEndpoint::External(_) | GraphEndpoint::Domain(_) => Ok(Vec::new()),
        }
    }

    /// The gaps that named this target among their persisted
    /// candidates. The only attribution a reverse query can make
    /// without guessing.
    fn candidate_gaps(
        &self,
        endpoint: &GraphEndpoint,
        kinds: &[RelationKind],
        states: &mut StateCache,
    ) -> Result<Vec<RelationGap>, RelationError> {
        let Some(entity_id) = graph::entity_id_of(&self.connection, endpoint)? else {
            return Ok(Vec::new());
        };
        self.gaps_where(
            "unresolved_reference.id IN \
             (SELECT unresolved_reference_id FROM relation_candidate WHERE target_entity_id = ?1)",
            &entity_id,
            kinds,
            states,
        )
    }

    /// How many unresolved sites of a matching intended kind exist at
    /// all. What keeps a reverse zero from reading as "none".
    fn unattributed_gaps(&self, kinds: &[RelationKind]) -> Result<usize, RelationError> {
        let sql = "SELECT COUNT(*) FROM unresolved_reference \
                   JOIN occurrence ON occurrence.id = unresolved_reference.occurrence_id \
                   JOIN resource ON resource.id = occurrence.resource_id \
                   WHERE resource.state = 'ACTIVE'";
        let mut statement = self.connection.prepare(sql)?;
        let total: i64 = statement.query_row([], |row| row.get(0))?;
        if kinds.is_empty() {
            return Ok(usize::try_from(total).unwrap_or(0));
        }
        // Intended kinds are few; filtering them in Rust keeps one
        // decode of the closed vocabulary instead of a second one in SQL.
        let mut statement = self.connection.prepare(
            "SELECT unresolved_reference.intended_relation_kind FROM unresolved_reference \
             JOIN occurrence ON occurrence.id = unresolved_reference.occurrence_id \
             JOIN resource ON resource.id = occurrence.resource_id \
             WHERE resource.state = 'ACTIVE'",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut matching = 0;
        for row in rows {
            if intended_matches(IntendedRelation::parse(&row?)?, kinds) {
                matching += 1;
            }
        }
        Ok(matching)
    }

    fn gaps_where<P: rusqlite::ToSql>(
        &self,
        predicate: &str,
        value: &P,
        kinds: &[RelationKind],
        states: &mut StateCache,
    ) -> Result<Vec<RelationGap>, RelationError> {
        let sql = format!(
            "SELECT resource.uid, resource.resource_revision, symbol.uid, occurrence.kind, \
                    occurrence.start_byte, occurrence.end_byte, occurrence.start_line, \
                    occurrence.start_col, occurrence.end_line, occurrence.end_col, \
                    occurrence.resource_revision, unresolved_reference.id, \
                    unresolved_reference.intended_relation_kind, \
                    unresolved_reference.lookup_name, unresolved_reference.module_hint, \
                    unresolved_reference.reason, unresolved_reference.candidate_truncated, \
                    resolution_context.context_key \
             FROM unresolved_reference \
             JOIN occurrence ON occurrence.id = unresolved_reference.occurrence_id \
             JOIN resource ON resource.id = occurrence.resource_id \
             LEFT JOIN symbol ON symbol.id = occurrence.containing_symbol_id \
             LEFT JOIN resolution_context \
                    ON resolution_context.id = unresolved_reference.resolution_context_id \
             WHERE resource.state = 'ACTIVE' AND {predicate} \
             ORDER BY resource.path_key, occurrence.start_byte, occurrence.end_byte, \
                      occurrence.kind"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(params![value], |row| {
            Ok((
                raw_location_row(row)?,
                row.get::<_, i64>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, String>(13)?,
                row.get::<_, Option<String>>(14)?,
                row.get::<_, String>(15)?,
                row.get::<_, i64>(16)?,
                row.get::<_, Option<String>>(17)?,
            ))
        })?;

        let mut found = Vec::new();
        for row in rows {
            let raw = row?;
            let intended = IntendedRelation::parse(&raw.2)?;
            if !kinds.is_empty() && !intended_matches(intended, kinds) {
                continue;
            }
            let candidates = self.candidates_of(raw.1)?;
            found.push(RelationGap {
                location: self.decode_location(raw.0, states)?,
                intended,
                lookup_name: raw.3,
                module_hint: raw.4,
                reason: UnresolvedReason::parse(&raw.5)?,
                resolution: if candidates.is_empty() {
                    Resolution::Unresolved
                } else {
                    // Having candidates is not having an answer.
                    Resolution::Candidate
                },
                candidates,
                candidate_truncated: raw.6 != 0,
                resolution_context_key: raw.7,
            });
        }
        Ok(found)
    }

    fn candidates_of(&self, unresolved_id: i64) -> Result<Vec<GraphEndpoint>, RelationError> {
        let mut statement = self.connection.prepare(
            "SELECT target_entity_id FROM relation_candidate \
             WHERE unresolved_reference_id = ?1 ORDER BY ordinal",
        )?;
        let rows = statement.query_map(params![unresolved_id], |row| row.get::<_, i64>(0))?;
        let mut found = Vec::new();
        for row in rows {
            found.push(graph::endpoint_of_entity(&self.connection, row?)?);
        }
        Ok(found)
    }

    /// One Symbol's storage anchor: its row id, and the Resource that
    /// declares it. Both stay inside this module.
    fn symbol_anchor(&self, symbol: SymbolId) -> Result<Option<(i64, ResourceId)>, RelationError> {
        Ok(self
            .connection
            .query_row(
                "SELECT symbol.id, resource.uid FROM symbol \
                 JOIN resource ON resource.id = symbol.resource_id \
                 WHERE symbol.uid = ?1 AND resource.state = 'ACTIVE'",
                params![symbol.to_bytes().to_vec()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?
            .map(|(row_id, uid)| (row_id, resource_id_of(&uid))))
    }

    fn scope_state(
        &self,
        resource: ResourceId,
        states: &mut StateCache,
    ) -> Result<ScopeState, RelationError> {
        let (support, component_current) = self.resource_state(resource, states)?;
        let revision: String = self.connection.query_row(
            "SELECT resource_revision FROM resource WHERE uid = ?1",
            params![resource.to_bytes().to_vec()],
            |row| row.get(0),
        )?;
        Ok(ScopeState {
            resource,
            support,
            // The scope is read against itself: what is left to say is
            // whether the index for it is current.
            freshness: freshness_of(&revision, &revision, component_current),
        })
    }

    /// One Resource's derived support, and whether the index behind
    /// its relations is current. Cached: a query asks once per
    /// Resource, not once per evidence span.
    ///
    /// Two components have to be current for a relation answer to be:
    /// the structure the evidence anchors to, and the `RELATION_INDEX`
    /// publication itself (#17 task 13). A DIRTY relation component
    /// still returns its last valid edges -- withholding them would be
    /// a false zero -- but they are not current truth, so the derived
    /// [`Freshness`] is DIRTY and no complete-negative claim survives
    /// it (#17 task 14).
    ///
    /// A Resource with no `RELATION_INDEX` row at all has no relation
    /// publication to be dirty about: a file that states no relations
    /// is never published (#17 task 13's fast path), and calling that
    /// not-current would make every relation-free file permanently
    /// incomplete.
    fn resource_state(
        &self,
        resource: ResourceId,
        states: &mut StateCache,
    ) -> Result<(Support, bool), RelationError> {
        if let Some(cached) = states.get(&resource) {
            return Ok(*cached);
        }
        let structure = structural::read(&self.connection, resource).map_err(|error| {
            RelationError::Structural {
                detail: error.to_string(),
            }
        })?;
        let relation_index =
            graph_lifecycle::state_of(&self.connection, resource).map_err(|error| {
                RelationError::Structural {
                    detail: error.to_string(),
                }
            })?;
        let state = (
            support_of(structure.as_ref().map(|structure| structure.state)),
            structure.is_some_and(|structure| {
                structure.freshness_state == crate::component::FreshnessState::Current
            }) && relation_index
                .is_none_or(|state| state == crate::component::FreshnessState::Current),
        );
        states.insert(resource, state);
        Ok(state)
    }

    fn decode_location(
        &self,
        raw: RawLocationRow,
        states: &mut StateCache,
    ) -> Result<EvidenceLocation, RelationError> {
        let resource = resource_id_of(&raw.0);
        let (support, component_current) = self.resource_state(resource, states)?;
        Ok(EvidenceLocation {
            resource,
            containing_symbol: raw.2.as_deref().map(symbol_id_of),
            occurrence_kind: OccurrenceKind::parse_public(&raw.3)?,
            span: SourceSpan {
                start_byte: as_usize(raw.4),
                end_byte: as_usize(raw.5),
                start: SourcePoint {
                    line: as_usize(raw.6),
                    column: as_usize(raw.7),
                },
                end: SourcePoint {
                    line: as_usize(raw.8),
                    column: as_usize(raw.9),
                },
            },
            support,
            freshness: freshness_of(&raw.10, &raw.1, component_current),
            basis_revision: raw.10,
        })
    }
}

type StateCache = HashMap<ResourceId, (Support, bool)>;

/// resource uid, resource revision, containing symbol uid, occurrence
/// kind, span, and the revision the evidence was extracted from.
type RawLocationRow = (
    Vec<u8>,
    String,
    Option<Vec<u8>>,
    String,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    String,
);

fn raw_location_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawLocationRow> {
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
    ))
}

/// Whether an unresolved site's intended relation is one of the kinds
/// asked for. An inheritance entry whose target is unknown answers to
/// both EXTENDS and IMPLEMENTS, because which one it is depends on that
/// target (#17 task 6).
fn intended_matches(intended: IntendedRelation, kinds: &[RelationKind]) -> bool {
    match intended {
        IntendedRelation::Known(kind) => kinds.contains(&kind),
        IntendedRelation::Inheritance => {
            kinds.contains(&RelationKind::Extends) || kinds.contains(&RelationKind::Implements)
        }
    }
}

/// A relation's place in the deterministic order: kind, then both
/// endpoints by canonical identity.
type ResultOrder = (&'static str, (u8, Vec<u8>), (u8, Vec<u8>));

/// Deterministic result order, over canonical identity only.
fn result_order(result: &RelationResult) -> ResultOrder {
    (
        result.kind.as_str(),
        graph::endpoint_sort_key(&result.source),
        graph::endpoint_sort_key(&result.target),
    )
}

fn resource_id_of(uid: &[u8]) -> ResourceId {
    ResourceId::from_bytes(sixteen(uid))
}

fn symbol_id_of(uid: &[u8]) -> SymbolId {
    SymbolId::from_bytes(sixteen(uid))
}

fn sixteen(raw: &[u8]) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    let take = raw.len().min(16);
    bytes[..take].copy_from_slice(&raw[..take]);
    bytes
}

fn as_usize(raw: i64) -> usize {
    usize::try_from(raw).unwrap_or(0)
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
        evidence::{OccurrenceRef, RelationEvidence, replace_resource_graph},
        gaps::{MAX_CANDIDATES, UnresolvedEvidence},
        generation,
        graph::{ExternalEntity, GraphStore},
        resolution::EvidenceBasis,
        resource::ResourceStore,
        scan::BaselineScan,
        structural::StructuralState,
        symbol::SymbolStore,
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    /// Two resolvable calls to one target, an import, an unresolved
    /// member call, an unresolved import, and an unresolved type.
    const APP_TS: &str = "\
import { shared } from './shared'
import { missing } from './missing'

export function run(obj: Thing): number {
  obj.foo()
  return shared() + shared()
}
";

    /// A second prover of the same edge, a value reference, and an
    /// external import.
    const OTHER_TS: &str = "\
import { shared } from './shared'
import { useState } from 'react'

export function other(): number {
  register(shared)
  return shared()
}
";

    const SHARED_TS: &str = "\
export function shared(): number {
  return 1
}

export function foo(): number {
  return 2
}
";

    const BASE_TS: &str = "export class Base {}\n";

    const TYPES_TS: &str = "\
import { Base } from './base'

export interface Shape {
  go(): number
}

export class Child extends Base implements Shape {
  go(input: Base): number {
    return 1
  }
}
";

    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-relations-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            fs::create_dir_all(root.join("src")).expect("src");
            let fixture = Self { base, root };
            fixture.write("src/app.ts", APP_TS);
            fixture.write("src/other.ts", OTHER_TS);
            fixture.write("src/shared.ts", SHARED_TS);
            fixture.write("src/base.ts", BASE_TS);
            fixture.write("src/types.ts", TYPES_TS);
            // Real indexed declarations, so a candidate set is canonical
            // identity rather than invented ids.
            let many: String = (0..MAX_CANDIDATES + 5)
                .map(|index| {
                    format!("export function f{index}(): number {{\n  return {index}\n}}\n")
                })
                .collect();
            fixture.write("src/many.ts", &many);
            BaselineScan::open(&fixture.db_path())
                .expect("index.db")
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

        fn resource(&self, rel: &str) -> crate::resource::Resource {
            ResourceStore::open(&self.db_path())
                .expect("index.db")
                .get_active_by_path_key(rel)
                .expect("lookup")
                .expect("the fixture file is a Resource")
        }

        fn symbol(&self, rel: &str, qualified_name: &str) -> SymbolId {
            SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_for_resource(self.resource(rel).id)
                .expect("symbols")
                .into_iter()
                .find(|symbol| symbol.qualified_name == qualified_name)
                .expect("the declaration is indexed")
                .id
        }

        fn many_symbols(&self) -> Vec<GraphEndpoint> {
            SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_for_resource(self.resource("src/many.ts").id)
                .expect("symbols")
                .into_iter()
                .map(|symbol| GraphEndpoint::Symbol(symbol.id))
                .collect()
        }

        /// One Resource's Occurrences of a kind, in source order.
        fn sites(&self, rel: &str, kind: OccurrenceKind) -> Vec<OccurrenceRef> {
            SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_occurrences_for_resource(self.resource(rel).id)
                .expect("occurrences")
                .into_iter()
                .filter(|occurrence| occurrence.kind == kind)
                .map(|occurrence| OccurrenceRef {
                    kind: occurrence.kind,
                    start_byte: occurrence.span.start_byte,
                    end_byte: occurrence.span.end_byte,
                })
                .collect()
        }

        fn index(&self) -> RelationIndex {
            RelationIndex::open(&self.db_path()).expect("index.db")
        }

        fn store(&self) -> GraphStore {
            GraphStore::open(&self.db_path()).expect("index.db")
        }

        fn basis(&self, rel: &str, generation_id: i64) -> EvidenceBasis {
            let resource = self.resource(rel);
            let profile_id = SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_for_resource(resource.id)
                .expect("symbols")
                .first()
                .expect("the file declares something")
                .analysis_profile_id;
            EvidenceBasis {
                owner_resource: resource.id,
                owner_resource_revision: resource.resource_revision,
                generation_id,
                analysis_profile_id: profile_id,
                resolution_context_key: None,
            }
        }

        /// Publish every owner's evidence in one real publication, the
        /// way #16 task 13/14 publish.
        fn publish(&self, plan: impl Fn(i64) -> Vec<OwnerEvidence>) {
            let store = self.store();
            let connection = store.connection();
            let revision = generation::current_workspace_revision(connection)
                .expect("clock")
                .expect("bootstrapped");
            let building = generation::begin_generation(connection, &revision).expect("begin");
            let transaction = connection.unchecked_transaction().expect("transaction");
            let (record, grant) =
                generation::grant_publication(&transaction, building.id).expect("grant");
            for owner in plan(building.id) {
                for item in &owner.resolved {
                    for endpoint in [&item.relation.source, &item.relation.target] {
                        graph::ensure_entity(&transaction, endpoint).expect("ensure");
                    }
                }
                for gap in &owner.unresolved {
                    for candidate in &gap.candidates {
                        graph::ensure_entity(&transaction, candidate).expect("ensure");
                    }
                }
                replace_resource_graph(
                    &transaction,
                    &grant,
                    &self.basis(&owner.rel, building.id),
                    &owner.resolved,
                    &owner.unresolved,
                )
                .expect("replace");
            }
            generation::finish_publish_stable(&transaction, &record).expect("stable");
            transaction.commit().expect("commit");
        }

        /// The whole fixture graph: resolved edges for app/other/types,
        /// and every gap shape on app.ts.
        fn publish_baseline(&self) {
            let app = GraphEndpoint::Resource(self.resource("src/app.ts").id);
            let other = GraphEndpoint::Resource(self.resource("src/other.ts").id);
            let shared_file = GraphEndpoint::Resource(self.resource("src/shared.ts").id);
            let run = GraphEndpoint::Symbol(self.symbol("src/app.ts", "run"));
            let other_fn = GraphEndpoint::Symbol(self.symbol("src/other.ts", "other"));
            let shared = GraphEndpoint::Symbol(self.symbol("src/shared.ts", "shared"));
            let child = GraphEndpoint::Symbol(self.symbol("src/types.ts", "Child"));
            let go = GraphEndpoint::Symbol(self.symbol("src/types.ts", "Child.go"));
            let shape = GraphEndpoint::Symbol(self.symbol("src/types.ts", "Shape"));
            let base = GraphEndpoint::Symbol(self.symbol("src/base.ts", "Base"));

            let app_calls = self.sites("src/app.ts", OccurrenceKind::CallSite);
            let app_imports = self.sites("src/app.ts", OccurrenceKind::ImportSite);
            let app_types = self.sites("src/app.ts", OccurrenceKind::TypeSite);
            let other_calls = self.sites("src/other.ts", OccurrenceKind::CallSite);
            let other_imports = self.sites("src/other.ts", OccurrenceKind::ImportSite);
            let other_refs = self.sites("src/other.ts", OccurrenceKind::ReferenceSite);
            let type_sites = self.sites("src/types.ts", OccurrenceKind::TypeSite);
            let candidates = self.many_symbols();

            self.publish(move |generation| {
                vec![
                    OwnerEvidence {
                        rel: "src/app.ts".to_owned(),
                        resolved: vec![
                            // Two call sites, one canonical edge.
                            evidence(
                                app_calls[1],
                                relation(RelationKind::Calls, &run, &shared, generation),
                            ),
                            evidence(
                                app_calls[2],
                                relation(RelationKind::Calls, &run, &shared, generation),
                            ),
                            evidence(
                                app_imports[0],
                                relation(RelationKind::Imports, &app, &shared_file, generation),
                            ),
                        ],
                        unresolved: vec![
                            UnresolvedEvidence {
                                occurrence: app_calls[0],
                                intended: IntendedRelation::Known(RelationKind::Calls),
                                lookup_name: "foo".to_owned(),
                                module_hint: None,
                                reason: UnresolvedReason::ReceiverTypeRequired,
                                candidates: Vec::new(),
                            },
                            UnresolvedEvidence {
                                occurrence: app_imports[1],
                                intended: IntendedRelation::Known(RelationKind::Imports),
                                lookup_name: "./missing".to_owned(),
                                module_hint: None,
                                reason: UnresolvedReason::CompoundSpecifier,
                                candidates: Vec::new(),
                            },
                            UnresolvedEvidence {
                                occurrence: app_types[0],
                                intended: IntendedRelation::Known(RelationKind::UsesType),
                                lookup_name: "Thing".to_owned(),
                                module_hint: None,
                                reason: UnresolvedReason::AmbiguousCandidates,
                                candidates: candidates.clone(),
                            },
                        ],
                    },
                    OwnerEvidence {
                        rel: "src/other.ts".to_owned(),
                        resolved: vec![
                            evidence(
                                other_calls[1],
                                relation(RelationKind::Calls, &other_fn, &shared, generation),
                            ),
                            evidence(
                                other_refs[0],
                                relation(RelationKind::References, &other_fn, &shared, generation),
                            ),
                            evidence(
                                other_imports[0],
                                relation(RelationKind::Imports, &other, &shared_file, generation),
                            ),
                            evidence(
                                other_imports[1],
                                relation(RelationKind::Imports, &other, &react(), generation),
                            ),
                        ],
                        unresolved: Vec::new(),
                    },
                    OwnerEvidence {
                        rel: "src/types.ts".to_owned(),
                        resolved: vec![
                            evidence(
                                type_sites[0],
                                relation(RelationKind::Extends, &child, &base, generation),
                            ),
                            evidence(
                                type_sites[1],
                                relation(RelationKind::Implements, &child, &shape, generation),
                            ),
                            evidence(
                                type_sites[2],
                                relation(RelationKind::UsesType, &go, &base, generation),
                            ),
                        ],
                        unresolved: Vec::new(),
                    },
                ]
            });
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    struct OwnerEvidence {
        rel: String,
        resolved: Vec<RelationEvidence>,
        unresolved: Vec<UnresolvedEvidence>,
    }

    fn relation(
        kind: RelationKind,
        source: &GraphEndpoint,
        target: &GraphEndpoint,
        generation: i64,
    ) -> Relation {
        Relation {
            kind,
            source: source.clone(),
            target: target.clone(),
            dispatch: Dispatch::Static,
            created_generation: generation,
        }
    }

    fn evidence(occurrence: OccurrenceRef, relation: Relation) -> RelationEvidence {
        RelationEvidence {
            occurrence,
            relation,
        }
    }

    fn react() -> GraphEndpoint {
        GraphEndpoint::External(ExternalEntity {
            package_identity: "react".to_owned(),
            module_path: None,
            symbol_name: Some("useState".to_owned()),
            qualified_name: None,
            kind: "IMPORTED_NAME".to_owned(),
            resolved_version: None,
            declaration_locator: None,
        })
    }

    /// Every relation kind stored, so a test can assert what the graph
    /// does *not* contain.
    fn stored_kinds(fixture: &Fixture) -> Vec<String> {
        let store = fixture.store();
        let mut statement = store
            .connection()
            .prepare("SELECT kind FROM relation ORDER BY kind")
            .expect("prepare");
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query");
        rows.map(|row| row.expect("kind")).collect()
    }

    #[test]
    fn outgoing_calls_returns_the_canonical_target_and_every_evidence_location() {
        let fixture = Fixture::create("outgoing-calls");
        fixture.publish_baseline();
        let run = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"));
        let shared = GraphEndpoint::Symbol(fixture.symbol("src/shared.ts", "shared"));

        let answer = fixture.index().callees(&run).expect("callees");

        assert_eq!(answer.confirmed_count(), 1, "two call sites, one edge");
        let result = &answer.confirmed[0];
        assert_eq!(result.kind, RelationKind::Calls);
        assert_eq!(result.source, run);
        assert_eq!(result.target, shared);
        assert_eq!(result.direction, Direction::Outgoing);
        assert_eq!(result.dispatch, Dispatch::Static);
        assert_eq!(result.target_scope, TargetScope::Internal);
        assert_eq!(result.resolution, Resolution::Resolved);
        assert_eq!(result.evidence.len(), 2, "both call sites are kept");

        // The spans are the published Occurrences', exactly.
        let published = fixture.sites("src/app.ts", OccurrenceKind::CallSite);
        let owner = fixture.resource("src/app.ts").id;
        for (location, site) in result.evidence.iter().zip(&published[1..]) {
            assert_eq!(location.resource, owner);
            assert_eq!(location.occurrence_kind, OccurrenceKind::CallSite);
            assert_eq!(location.span.start_byte, site.start_byte);
            assert_eq!(location.span.end_byte, site.end_byte);
            assert_eq!(
                location.containing_symbol,
                Some(fixture.symbol("src/app.ts", "run"))
            );
        }
        assert!(
            result.evidence[0].span.start_byte < result.evidence[1].span.start_byte,
            "evidence is ordered, not collapsed"
        );
    }

    #[test]
    fn callers_and_callees_are_two_views_of_one_stored_edge() {
        let fixture = Fixture::create("symmetry");
        fixture.publish_baseline();
        let index = fixture.index();
        let run = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"));
        let other = GraphEndpoint::Symbol(fixture.symbol("src/other.ts", "other"));
        let shared = GraphEndpoint::Symbol(fixture.symbol("src/shared.ts", "shared"));

        let callers = index.callers(&shared).expect("callers");
        assert_eq!(callers.confirmed_count(), 2);
        assert_eq!(callers.direction, Direction::Incoming);
        let sources: Vec<&GraphEndpoint> = callers
            .confirmed
            .iter()
            .map(|result| &result.source)
            .collect();
        assert!(sources.contains(&&run) && sources.contains(&&other));
        for result in &callers.confirmed {
            assert_eq!(result.target, shared, "the reverse read keeps direction");
            assert!(!result.evidence.is_empty(), "the caller's own sites");
        }

        // Same edge, read from the other end.
        let callees = index.callees(&run).expect("callees");
        let forward = &callees.confirmed[0];
        let reverse = callers
            .confirmed
            .iter()
            .find(|result| result.source == run)
            .expect("the same edge");
        assert_eq!(
            (forward.kind, &forward.source, &forward.target),
            (reverse.kind, &reverse.source, &reverse.target)
        );
        assert_eq!(forward.evidence, reverse.evidence);

        // Nothing reverse was stored to make that work.
        let kinds = stored_kinds(&fixture);
        assert!(!kinds.iter().any(|kind| kind.ends_with("_BY")), "{kinds:?}");
        assert_eq!(kinds.iter().filter(|kind| *kind == "CALLS").count(), 2);
    }

    #[test]
    fn imports_and_importers_are_two_views_of_one_stored_edge() {
        let fixture = Fixture::create("imports");
        fixture.publish_baseline();
        let index = fixture.index();
        let app = fixture.resource("src/app.ts").id;
        let other = GraphEndpoint::Resource(fixture.resource("src/other.ts").id);
        let shared_file = GraphEndpoint::Resource(fixture.resource("src/shared.ts").id);

        let imports = index.imports(app).expect("imports");
        assert_eq!(imports.confirmed_count(), 1);
        assert_eq!(imports.confirmed[0].target, shared_file);
        assert_eq!(
            imports.confirmed[0].evidence[0].occurrence_kind,
            OccurrenceKind::ImportSite
        );

        let importers = index.importers(&shared_file).expect("importers");
        assert_eq!(importers.confirmed_count(), 2);
        let sources: Vec<&GraphEndpoint> = importers
            .confirmed
            .iter()
            .map(|result| &result.source)
            .collect();
        assert!(sources.contains(&&GraphEndpoint::Resource(app)) && sources.contains(&&other));
        assert_eq!(
            stored_kinds(&fixture)
                .iter()
                .filter(|kind| *kind == "IMPORTS")
                .count(),
            3,
            "two internal edges and one external -- no IMPORTED_BY"
        );
    }

    #[test]
    fn incoming_references_returns_confirmed_referrers_only() {
        let fixture = Fixture::create("references");
        fixture.publish_baseline();
        let shared = GraphEndpoint::Symbol(fixture.symbol("src/shared.ts", "shared"));
        let other = GraphEndpoint::Symbol(fixture.symbol("src/other.ts", "other"));

        let answer = fixture.index().references(&shared).expect("references");

        assert_eq!(answer.confirmed_count(), 1);
        assert_eq!(answer.confirmed[0].source, other);
        assert_eq!(answer.confirmed[0].kind, RelationKind::References);
        assert_eq!(
            answer.confirmed[0].evidence[0].occurrence_kind,
            OccurrenceKind::ReferenceSite,
            "a value reference, not the call's own callee"
        );
        assert!(
            answer.gaps.is_empty(),
            "no unresolved site names this target as a candidate"
        );
    }

    #[test]
    fn direct_type_and_inheritance_relations_answer_per_kind() {
        let fixture = Fixture::create("types");
        fixture.publish_baseline();
        let index = fixture.index();
        let child = GraphEndpoint::Symbol(fixture.symbol("src/types.ts", "Child"));
        let go = GraphEndpoint::Symbol(fixture.symbol("src/types.ts", "Child.go"));
        let base = GraphEndpoint::Symbol(fixture.symbol("src/base.ts", "Base"));
        let shape = GraphEndpoint::Symbol(fixture.symbol("src/types.ts", "Shape"));

        let all = index
            .type_relations(&child, Direction::Outgoing, &[])
            .expect("type relations");
        assert_eq!(all.confirmed_count(), 2, "EXTENDS and IMPLEMENTS");
        assert_eq!(
            all.confirmed
                .iter()
                .map(|result| (result.kind, result.target.clone()))
                .collect::<Vec<_>>(),
            vec![
                (RelationKind::Extends, base.clone()),
                (RelationKind::Implements, shape.clone()),
            ]
        );

        let extends_only = index
            .type_relations(&child, Direction::Outgoing, &[RelationKind::Extends])
            .expect("extends");
        assert_eq!(extends_only.confirmed_count(), 1);
        assert_eq!(extends_only.confirmed[0].target, base);

        let uses = index
            .type_relations(&go, Direction::Outgoing, &[RelationKind::UsesType])
            .expect("uses type");
        assert_eq!(uses.confirmed[0].target, base);

        // Reverse: what depends on Base, through the same rows.
        let incoming = index
            .type_relations(&base, Direction::Incoming, &[])
            .expect("incoming");
        assert_eq!(incoming.confirmed_count(), 2);
        assert!(
            incoming
                .confirmed
                .iter()
                .all(|result| result.target == base && result.direction == Direction::Incoming)
        );

        assert!(matches!(
            index.type_relations(&child, Direction::Outgoing, &[RelationKind::Calls]),
            Err(RelationError::NotATypeKind { .. })
        ));
    }

    #[test]
    fn results_carry_stable_identity_and_never_a_storage_row_id() {
        let fixture = Fixture::create("identity");
        fixture.publish_baseline();
        let index = fixture.index();
        let other = GraphEndpoint::Resource(fixture.resource("src/other.ts").id);

        let answer = index
            .imports(fixture.resource("src/other.ts").id)
            .expect("imports");
        let external = answer
            .confirmed
            .iter()
            .find(|result| result.target_scope == TargetScope::External)
            .expect("the external import");
        assert_eq!(external.target, react(), "ExternalEntity round-trips");
        assert_eq!(external.source, other);

        // The external endpoint is usable as a query anchor, which it
        // could not be if identity did not round-trip.
        let importers = index.importers(&react()).expect("importers");
        assert_eq!(importers.confirmed_count(), 1);
        assert_eq!(importers.confirmed[0].source, other);

        // Nothing in the result contract can express a row id: the
        // endpoints are the stable ids the stores hold.
        let shared = fixture.symbol("src/shared.ts", "shared");
        let callers = index
            .callers(&GraphEndpoint::Symbol(shared))
            .expect("callers");
        for result in &callers.confirmed {
            assert!(matches!(
                result.source,
                GraphEndpoint::Symbol(_) | GraphEndpoint::Resource(_)
            ));
            for location in &result.evidence {
                assert!(
                    [
                        fixture.resource("src/app.ts").id,
                        fixture.resource("src/other.ts").id
                    ]
                    .contains(&location.resource)
                );
            }
        }
    }

    #[test]
    fn ordering_is_deterministic_and_carries_no_duplicates() {
        let fixture = Fixture::create("ordering");
        fixture.publish_baseline();
        let index = fixture.index();
        let shared = GraphEndpoint::Symbol(fixture.symbol("src/shared.ts", "shared"));

        let first = index.incoming(&shared, &[]).expect("incoming");
        let second = index.incoming(&shared, &[]).expect("incoming");
        assert_eq!(first, second);

        // Asking for the same kind twice must not double the answer.
        let repeated = index
            .incoming(&shared, &[RelationKind::Calls, RelationKind::Calls])
            .expect("incoming");
        assert_eq!(repeated.confirmed_count(), 2);

        let keys: Vec<(&str, (u8, Vec<u8>))> = first
            .confirmed
            .iter()
            .map(|result| {
                (
                    result.kind.as_str(),
                    graph::endpoint_sort_key(&result.source),
                )
            })
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "identity order, never row order");
    }

    #[test]
    fn unresolved_evidence_comes_back_as_a_gap_never_as_a_relation() {
        let fixture = Fixture::create("gaps");
        fixture.publish_baseline();
        let run = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"));

        let answer = fixture.index().callees(&run).expect("callees");

        assert_eq!(
            answer.confirmed_count(),
            1,
            "the member call is not confirmed"
        );
        assert_eq!(answer.gaps.len(), 1);
        let gap = &answer.gaps[0];
        assert_eq!(gap.intended, IntendedRelation::Known(RelationKind::Calls));
        assert_eq!(gap.lookup_name, "foo");
        assert_eq!(gap.reason, UnresolvedReason::ReceiverTypeRequired);
        assert_eq!(gap.resolution, Resolution::Unresolved);
        assert!(gap.candidates.is_empty());
        assert_eq!(gap.location.occurrence_kind, OccurrenceKind::CallSite);
        assert_eq!(
            gap.location.resource,
            fixture.resource("src/app.ts").id,
            "the gap keeps its exact anchor"
        );
        assert_eq!(answer.coverage.requires_semantics, 1, "an I4 question");
        assert!(!answer.coverage.is_complete());
        assert!(!answer.confirmed_zero_is_none());
    }

    #[test]
    fn an_unsupported_construct_gap_is_visible_on_the_owning_resource() {
        let fixture = Fixture::create("unsupported");
        fixture.publish_baseline();
        let app = fixture.resource("src/app.ts").id;

        let answer = fixture.index().imports(app).expect("imports");

        assert_eq!(answer.confirmed_count(), 1);
        assert_eq!(answer.gaps.len(), 1);
        assert_eq!(answer.gaps[0].reason, UnresolvedReason::CompoundSpecifier);
        assert_eq!(answer.coverage.unsupported_construct, 1);
        assert_eq!(answer.coverage.requires_semantics, 0);
        assert!(
            !answer.confirmed_zero_is_none(),
            "one confirmed import is not the whole import list"
        );
    }

    #[test]
    fn ambiguous_candidates_stay_candidates_and_truncation_stays_visible() {
        let fixture = Fixture::create("candidates");
        fixture.publish_baseline();
        let index = fixture.index();
        let run = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"));

        let answer = index
            .outgoing(&run, &[RelationKind::UsesType])
            .expect("uses type");

        assert_eq!(answer.confirmed_count(), 0, "a candidate is not an edge");
        assert_eq!(answer.gaps.len(), 1);
        let gap = &answer.gaps[0];
        assert_eq!(gap.reason, UnresolvedReason::AmbiguousCandidates);
        assert_eq!(gap.resolution, Resolution::Candidate);
        assert_eq!(gap.candidates.len(), MAX_CANDIDATES);
        assert!(gap.candidate_truncated, "the list was cut, and it says so");
        assert_eq!(answer.coverage.ambiguous, 1);
        assert_eq!(answer.coverage.truncated, 1);

        // A candidate is reachable from the other end -- as a candidate.
        let candidate = gap.candidates[0].clone();
        let incoming = index
            .incoming(&candidate, &[RelationKind::UsesType])
            .expect("incoming");
        assert_eq!(incoming.confirmed_count(), 0);
        assert_eq!(incoming.gaps.len(), 1);
        assert_eq!(incoming.gaps[0].resolution, Resolution::Candidate);
        assert_eq!(
            incoming.coverage.attribution,
            GapAttribution::TargetCandidateOnly
        );
        assert!(incoming.gaps[0].candidates.contains(&candidate));
    }

    #[test]
    fn a_gap_with_no_target_is_never_attached_to_a_same_named_symbol() {
        let fixture = Fixture::create("no-name-match");
        fixture.publish_baseline();
        // app.ts has an unresolved `obj.foo()`; shared.ts declares `foo`.
        let foo = GraphEndpoint::Symbol(fixture.symbol("src/shared.ts", "foo"));

        let answer = fixture.index().callers(&foo).expect("callers");

        assert_eq!(answer.confirmed_count(), 0);
        assert!(
            answer.gaps.is_empty(),
            "the unresolved call names no target, so it is not this one's"
        );
        assert_eq!(
            answer.coverage.attribution,
            GapAttribution::TargetCandidateOnly
        );
        assert_eq!(answer.coverage.unattributed, 1, "counted, never attached");
        assert!(
            !answer.confirmed_zero_is_none(),
            "zero callers is not proof of none while a call site is unresolved"
        );
    }

    #[test]
    fn confirmed_zero_with_no_gap_is_distinguishable_from_zero_with_one() {
        let fixture = Fixture::create("false-zero");
        fixture.publish_baseline();
        let index = fixture.index();

        // shared.ts calls nothing, is fully covered, and states no gap.
        let quiet = index
            .callees(&GraphEndpoint::Symbol(
                fixture.symbol("src/shared.ts", "shared"),
            ))
            .expect("callees");
        assert_eq!(quiet.confirmed_count(), 0);
        assert!(quiet.gaps.is_empty());
        assert_eq!(
            quiet
                .coverage
                .scope
                .expect("a forward query has a scope")
                .support,
            Support::Supported
        );
        assert!(quiet.coverage.is_complete());
        assert!(quiet.confirmed_zero_is_none(), "this zero really is none");

        // run() has an unresolved type use: same zero, different answer.
        let noisy = index
            .outgoing(
                &GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run")),
                &[RelationKind::UsesType],
            )
            .expect("uses type");
        assert_eq!(noisy.confirmed_count(), 0);
        assert_eq!(noisy.coverage.gaps, 1);
        assert!(!noisy.coverage.is_complete());
        assert!(!noisy.confirmed_zero_is_none());
    }

    #[test]
    fn support_and_freshness_follow_the_derived_contracts() {
        let fixture = Fixture::create("axes");
        fixture.publish_baseline();
        let shared = GraphEndpoint::Symbol(fixture.symbol("src/shared.ts", "shared"));
        let app = fixture.resource("src/app.ts").id;
        let other = fixture.resource("src/other.ts").id;

        let fresh = fixture.index().callers(&shared).expect("callers");
        for result in &fresh.confirmed {
            assert_eq!(result.support, Support::Supported);
            assert_eq!(result.freshness, Freshness::Fresh);
        }

        // other.ts stops parsing cleanly: its evidence is partial cover
        // and no longer current.
        {
            let store = fixture.store();
            crate::structural::write(
                store.connection(),
                other,
                StructuralState::Partial,
                "workspace-rev-1",
                Some(1),
                None,
            )
            .expect("structural state");
            // app.ts moves on: its evidence is the last valid one.
            store
                .connection()
                .execute(
                    "UPDATE resource SET resource_revision = 'moved' WHERE uid = ?1",
                    params![app.to_bytes().to_vec()],
                )
                .expect("bump revision");
        }

        let answer = fixture.index().callers(&shared).expect("callers");
        let from_app = answer
            .confirmed
            .iter()
            .find(|result| result.evidence[0].resource == app)
            .expect("app.ts evidence");
        assert_eq!(from_app.freshness, Freshness::Stale, "the source moved on");
        assert_eq!(from_app.support, Support::Supported);
        let from_other = answer
            .confirmed
            .iter()
            .find(|result| result.evidence[0].resource == other)
            .expect("other.ts evidence");
        assert_eq!(from_other.support, Support::Partial);
        assert_eq!(from_other.freshness, Freshness::Dirty);

        // A partial owner cannot report a complete forward answer.
        let forward = fixture.index().imports(other).expect("imports");
        assert!(forward.gaps.is_empty());
        assert!(
            !forward.coverage.is_complete(),
            "partial coverage is not a confirmed whole"
        );
    }

    #[test]
    fn a_query_returns_locators_and_never_source_text() {
        let fixture = Fixture::create("no-source");
        fixture.publish_baseline();
        let run = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"));

        let answer = fixture.index().callees(&run).expect("callees");
        let rendered = format!("{answer:?}");

        assert!(!rendered.contains("return shared()"), "no source body");
        assert!(!rendered.contains("export function"), "no source body");
        let location = &answer.confirmed[0].evidence[0];
        assert!(location.span.end_byte > location.span.start_byte);
        assert!(location.span.end.line >= location.span.start.line);
    }
}
