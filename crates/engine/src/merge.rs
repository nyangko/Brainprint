//! Merging semantic evidence into the one canonical graph (#19 task 4).
//!
//! I3 leaves honest gaps and I4 closes them. What this module owns is
//! the step between: turning current, normalized
//! [`SemanticEvidence`] into changes to the Resource/Symbol/Occurrence/
//! Relation graph that I2 and I3 already publish.
//!
//! There is no second graph. A relation a backend proves is a `relation`
//! row like any other, anchored to the `occurrence` the structural tier
//! already recorded at that exact span. A backend agreeing with the
//! parser adds provenance, not a second edge; two call sites of one
//! function stay two occurrences of one edge, as they always were.
//!
//! ## Which tier proved what
//!
//! The one thing the canonical tables cannot say is *why* an edge is
//! there, and that is exactly what a downgrade needs to know:
//!
//! - [`ProofRole::Binding`] -- the edge exists because semantic evidence
//!   resolved it. If that evidence stops being current, the edge stops
//!   being claimed and the gap it displaced comes back.
//! - [`ProofRole::Corroborating`] -- the parser already proved it and
//!   the backend agrees. The backend disappearing costs nothing.
//!
//! So `min(structural, semantic)` is not the freshness rule. A
//! structurally proven edge stays current while its redundant semantic
//! confirmation goes stale; a semantically bound one does not.
//!
//! ## Disagreement is not a vote
//!
//! When the parser confirms `A` and the backend confirms `B` for the
//! same site, neither wins. The structural edge is not deleted -- it is
//! still structurally proven -- the semantic target does not become a
//! canonical edge, and a [`SemanticConflict`] records both so that
//! [`CoverageLimit::SemanticConflict`] can stop any query from reading
//! that scope as a clean answer. There is no score anywhere that picks a
//! winner.
//!
//! ## What this module refuses to do
//!
//! It never invents an Occurrence. Semantic evidence that cannot be
//! anchored to an existing Occurrence at the Resource's current revision
//! is rejected and reported, because a relation whose source site cannot
//! be pointed at is a claim about nothing. It never infers a relation
//! kind the evidence did not state. It never persists a backend's own
//! identity, a request id, or a raw response -- what it writes is
//! Brainprint identity, a capability name, and a profile reference.

use std::collections::BTreeMap;

use brainprint_core::ResourceId;
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    evidence::OccurrenceRef,
    gaps::{GapError, IntendedRelation, UnresolvedReason},
    generation::PublicationGrant,
    graph::{self, GraphEndpoint, GraphError, Relation, RelationKind},
    resource::ResourceError,
    semantic::{AnalysisContext, SemanticCapability, SemanticEvidence, SemanticOutcome},
    semantic_index::{SemanticState, SemanticStatus},
    symbol::OccurrenceKind,
};

// ---------------------------------------------------------------------
// Proof roles and records
// ---------------------------------------------------------------------

/// Why a canonical relation is there, from the semantic tier's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofRole {
    /// The relation exists because this semantic evidence resolved it:
    /// a structural gap closed, or an edge only a backend can prove.
    /// Withdraw the evidence and the edge goes with it.
    Binding,
    /// The structural tier already proved this edge. The semantic tier
    /// agrees, and its leaving changes nothing about the edge.
    Corroborating,
}

impl ProofRole {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Binding => "BINDING",
            Self::Corroborating => "CORROBORATING",
        }
    }

    fn parse(raw: &str) -> Result<Self, MergeError> {
        match raw {
            "BINDING" => Ok(Self::Binding),
            "CORROBORATING" => Ok(Self::Corroborating),
            other => Err(MergeError::UnknownProofRole {
                raw: other.to_owned(),
            }),
        }
    }
}

/// The structural gap a binding semantic proof displaced.
///
/// Kept so that withdrawing the proof restores an honest gap instead of
/// a silence. The candidate list is deliberately not kept: candidates
/// are the structural resolver's own working set, and the next
/// re-extraction produces them again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplacedGap {
    pub intended: IntendedRelation,
    pub lookup_name: String,
    pub module_hint: Option<String>,
    pub reason: UnresolvedReason,
}

/// Two tiers, one site, two different confirmed targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticConflict {
    pub context_key: String,
    pub occurrence: OccurrenceRef,
    pub kind: RelationKind,
    pub source: GraphEndpoint,
    /// What the structural tier proved, and which is still the bound
    /// canonical edge.
    pub structural_target: GraphEndpoint,
    /// What the semantic backend proved, which is recorded and not
    /// promoted to an edge.
    pub semantic_target: GraphEndpoint,
}

/// Why one piece of semantic evidence was not merged.
///
/// Rejections are returned, never silently dropped: a backend answer
/// this tier cannot anchor is a fact about coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionReason {
    /// The evidence names no source span.
    NoOccurrence,
    /// No Occurrence exists at that span for this Resource. Nothing is
    /// invented to hold the relation.
    UnanchoredOccurrence,
    /// The Occurrence describes a revision the Resource has moved past.
    StaleOccurrence,
    /// The evidence states no relation kind, and one is never inferred
    /// from the capability.
    NoRelationKind,
    /// The evidence names no source endpoint.
    NoSource,
    /// The outcome is a candidate set or an unresolved site. Semantic
    /// candidates are not promoted: a target is proven or it is not.
    NotResolved,
    /// The Occurrence already binds a canonical relation of a different
    /// kind, and an Occurrence binds one relation. The structural
    /// binding is kept.
    OccurrenceBoundElsewhere,
}

/// One rejected item, with the site it was about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedEvidence {
    pub capability: SemanticCapability,
    pub occurrence: Option<OccurrenceRef>,
    pub reason: RejectionReason,
}

/// What one merge changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeOutcome {
    /// Gaps (unresolved or candidate sites) that semantic evidence
    /// resolved.
    pub gaps_resolved: usize,
    /// Gaps restored because a previous binding proof was withdrawn.
    pub gaps_restored: usize,
    /// Edges the structural tier already proved and the backend
    /// confirmed.
    pub corroborated: usize,
    /// Canonical relation rows newly inserted.
    pub relations_created: usize,
    /// Canonical relation rows removed because their last supporting
    /// evidence went away.
    pub relations_removed: usize,
    pub conflicts: usize,
    pub rejected: Vec<RejectedEvidence>,
}

/// A scope's semantic coverage facts, for [`crate::coverage`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SemanticScope {
    /// Semantic contexts contributing evidence to this scope.
    pub contexts: usize,
    pub conflicts: usize,
    /// At least one contributing context is not CURRENT, so what is
    /// shown is the last valid semantic answer.
    pub not_current: bool,
}

impl SemanticScope {
    /// Whether anything semantic has anything to say about this scope.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.contexts == 0 && self.conflicts == 0 && !self.not_current
    }
}

// ---------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------

#[derive(Debug)]
pub enum MergeError {
    Sqlite(rusqlite::Error),
    Graph(GraphError),
    Gap(GapError),
    Resource(ResourceError),
    /// The semantic publication is not CURRENT, so it may not change
    /// canonical current truth. Nothing was written.
    NotCurrent {
        context_key: String,
        state: SemanticState,
    },
    /// A piece of evidence names a different AnalysisContext than the
    /// merge is for. One context never writes into another's
    /// contribution.
    ContextMismatch {
        expected: String,
        found: String,
    },
    /// A piece of evidence names a different owner Resource than the
    /// merge is for.
    OwnerMismatch {
        expected: ResourceId,
        found: ResourceId,
    },
    UnknownResource {
        resource: ResourceId,
    },
    UnknownProofRole {
        raw: String,
    },
}

impl std::fmt::Display for MergeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(source) => write!(formatter, "semantic merge sqlite error: {source}"),
            Self::Graph(source) => write!(formatter, "graph failed: {source}"),
            Self::Gap(source) => write!(formatter, "gap decode failed: {source}"),
            Self::Resource(source) => write!(formatter, "resource store failed: {source}"),
            Self::NotCurrent { context_key, state } => write!(
                formatter,
                "semantic publication for {context_key} is {state}, not current"
            ),
            Self::ContextMismatch { expected, found } => write!(
                formatter,
                "evidence for context {found} cannot merge into {expected}"
            ),
            Self::OwnerMismatch { expected, found } => write!(
                formatter,
                "evidence owned by {found} cannot merge into {expected}'s contribution"
            ),
            Self::UnknownResource { resource } => {
                write!(formatter, "resource {resource} is not indexed")
            }
            Self::UnknownProofRole { raw } => write!(formatter, "unknown proof role {raw:?}"),
        }
    }
}

impl std::error::Error for MergeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(source) => Some(source),
            Self::Graph(source) => Some(source),
            Self::Resource(source) => Some(source),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for MergeError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

impl From<GraphError> for MergeError {
    fn from(source: GraphError) -> Self {
        Self::Graph(source)
    }
}

impl From<GapError> for MergeError {
    fn from(source: GapError) -> Self {
        Self::Gap(source)
    }
}

impl From<ResourceError> for MergeError {
    fn from(source: ResourceError) -> Self {
        Self::Resource(source)
    }
}

// ---------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------

/// One Resource's semantic contribution from one AnalysisContext.
///
/// The unit of replacement: merging replaces everything this context
/// previously contributed for this Resource, and touches nothing else.
pub struct MergeRequest<'a> {
    pub context: &'a AnalysisContext,
    /// The publication's freshness, from #19 task 3. Only CURRENT may
    /// change canonical current truth.
    pub status: &'a SemanticStatus,
    pub owner: ResourceId,
    /// The semantic `analysis_profile` row this contribution was
    /// produced under. A reference, so provenance is not a version tuple
    /// copied onto every row.
    pub analysis_profile_id: i64,
    pub evidence: &'a [SemanticEvidence],
}

// ---------------------------------------------------------------------
// Merge
// ---------------------------------------------------------------------

/// Apply one context's semantic contribution for one Resource, in the
/// caller's open publication transaction.
///
/// Replacement, not accumulation: whatever this context contributed for
/// this Resource before is reconciled against what the evidence says
/// now. An edge that is still proven stays exactly as it is, which is
/// what makes applying the same publication twice a no-op.
pub fn merge(
    connection: &Connection,
    grant: &PublicationGrant,
    request: &MergeRequest<'_>,
) -> Result<MergeOutcome, MergeError> {
    let context_key = request.context.context_key();
    if request.status.state != SemanticState::Current {
        // BUILDING, DIRTY and UNAVAILABLE evidence does not touch
        // canonical current truth. Structural truth stands and the
        // coverage says why.
        return Err(MergeError::NotCurrent {
            context_key,
            state: request.status.state,
        });
    }
    for item in request.evidence {
        if item.context_key != context_key {
            return Err(MergeError::ContextMismatch {
                expected: context_key,
                found: item.context_key.clone(),
            });
        }
        if item.basis.owner_resource != request.owner {
            return Err(MergeError::OwnerMismatch {
                expected: request.owner,
                found: item.basis.owner_resource,
            });
        }
    }

    let owner = owner_row(connection, request.owner)?;
    let revision = owner_revision(connection, owner)?;
    let mut outcome = MergeOutcome::default();

    // Conflicts carry no displaced state, so they are replaced whole.
    clear_conflicts(connection, &context_key, owner)?;

    let previous = load_contribution(connection, &context_key, owner)?;
    let mut kept: BTreeMap<i64, ()> = BTreeMap::new();
    let mut touched_relations: Vec<i64> = Vec::new();

    for item in request.evidence {
        match plan(connection, &context_key, owner, &revision, item)? {
            Planned::Rejected(rejection) => outcome.rejected.push(rejection),
            Planned::Conflict {
                occurrence_id,
                conflict,
            } => {
                write_conflict(
                    connection,
                    occurrence_id,
                    &conflict,
                    request.analysis_profile_id,
                    grant,
                )?;
                outcome.conflicts += 1;
            }
            Planned::Apply(applied) => {
                let PlannedApply {
                    occurrence_id,
                    mut relation,
                    role,
                    capability,
                } = *applied;
                // The generation publishing this merge is the one that
                // created any edge it introduces.
                relation.created_generation = grant.generation_id();
                if let Some(existing) = previous.get(&occurrence_id) {
                    // The same proof as last time: leave the graph
                    // exactly as it is.
                    if relation_row(connection, &relation)? == Some(existing.relation_id) {
                        kept.insert(occurrence_id, ());
                        upsert_semantic_evidence(
                            connection,
                            &context_key,
                            occurrence_id,
                            existing.relation_id,
                            capability,
                            role,
                            existing.displaced.as_ref(),
                            request.analysis_profile_id,
                            grant,
                        )?;
                        continue;
                    }
                    // A different proof: withdraw the old one first, so
                    // an old semantic target cannot survive alongside
                    // the new one.
                    let restored = withdraw_one(connection, &context_key, occurrence_id, existing)?;
                    outcome.gaps_restored += usize::from(restored);
                    touched_relations.push(existing.relation_id);
                }

                graph::ensure_entity(connection, &relation.source)?;
                graph::ensure_entity(connection, &relation.target)?;
                if graph::insert_relation(connection, &relation)? {
                    outcome.relations_created += 1;
                }
                let relation_id = require_relation_row(connection, &relation)?;

                let displaced = match role {
                    ProofRole::Binding => {
                        let displaced = take_gap(connection, occurrence_id)?;
                        if displaced.is_some() {
                            outcome.gaps_resolved += 1;
                        }
                        bind_occurrence(connection, occurrence_id, relation_id)?;
                        displaced
                    }
                    ProofRole::Corroborating => {
                        outcome.corroborated += 1;
                        None
                    }
                };
                upsert_semantic_evidence(
                    connection,
                    &context_key,
                    occurrence_id,
                    relation_id,
                    capability,
                    role,
                    displaced.as_ref(),
                    request.analysis_profile_id,
                    grant,
                )?;
                kept.insert(occurrence_id, ());
            }
        }
    }

    // Anything this context proved last time and does not prove now.
    for (occurrence_id, entry) in &previous {
        if kept.contains_key(occurrence_id) {
            continue;
        }
        let restored = withdraw_one(connection, &context_key, *occurrence_id, entry)?;
        outcome.gaps_restored += usize::from(restored);
        touched_relations.push(entry.relation_id);
    }

    outcome.relations_removed = collect_unevidenced(connection, &touched_relations)?;
    Ok(outcome)
}

/// Drop one context's whole semantic contribution for one Resource, or
/// for every Resource when `owner` is `None`.
///
/// What the backend proved on its own goes away and the gaps it
/// displaced come back; what the parser proved stays exactly where it
/// is. This is the honest state when a backend becomes unavailable or
/// its publication stops being current -- not an empty success, and not
/// a silently preserved "resolved".
pub fn withdraw(
    connection: &Connection,
    context_key: &str,
    owner: Option<ResourceId>,
) -> Result<MergeOutcome, MergeError> {
    let owner_id = match owner {
        Some(resource) => Some(owner_row(connection, resource)?),
        None => None,
    };
    let mut outcome = MergeOutcome::default();
    let previous = load_contribution_opt(connection, context_key, owner_id)?;
    let mut touched: Vec<i64> = Vec::new();

    for (occurrence_id, entry) in &previous {
        let restored = withdraw_one(connection, context_key, *occurrence_id, entry)?;
        outcome.gaps_restored += usize::from(restored);
        touched.push(entry.relation_id);
    }
    match owner_id {
        Some(owner) => clear_conflicts(connection, context_key, owner)?,
        None => {
            connection.execute(
                "DELETE FROM semantic_conflict WHERE context_key = ?1",
                params![context_key],
            )?;
        }
    }
    outcome.relations_removed = collect_unevidenced(connection, &touched)?;
    Ok(outcome)
}

/// `semantic_conflict` joined with its Occurrence, as stored: context,
/// occurrence kind and span, relation kind, and the three entities.
type RawConflictRow = (String, String, i64, i64, String, i64, i64, i64);

/// `semantic_evidence` as stored: occurrence, relation, proof role, and
/// the four fields of the gap it displaced.
type RawContributionRow = (
    i64,
    i64,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Every conflict recorded against one Resource, in deterministic
/// order.
pub fn conflicts_for_resource(
    connection: &Connection,
    resource: ResourceId,
) -> Result<Vec<SemanticConflict>, MergeError> {
    let mut statement = connection.prepare(
        "SELECT c.context_key, o.kind, o.start_byte, o.end_byte, c.relation_kind, \
                c.source_entity_id, c.structural_target_entity_id, c.semantic_target_entity_id \
         FROM semantic_conflict c \
         JOIN occurrence o ON o.id = c.occurrence_id \
         JOIN resource r ON r.id = o.resource_id \
         WHERE r.uid = ?1 \
         ORDER BY o.start_byte, o.end_byte, c.relation_kind, c.context_key",
    )?;
    let rows: Vec<RawConflictRow> = statement
        .query_map(params![resource.to_bytes().to_vec()], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    drop(statement);

    let mut found = Vec::new();
    for row in rows {
        found.push(SemanticConflict {
            context_key: row.0,
            occurrence: OccurrenceRef {
                kind: OccurrenceKind::parse_public(&row.1)
                    .map_err(|_| MergeError::UnknownProofRole { raw: row.1.clone() })?,
                start_byte: usize::try_from(row.2).unwrap_or(0),
                end_byte: usize::try_from(row.3).unwrap_or(0),
            },
            kind: RelationKind::parse(&row.4)?,
            source: graph::endpoint_of_entity(connection, row.5)?,
            structural_target: graph::endpoint_of_entity(connection, row.6)?,
            semantic_target: graph::endpoint_of_entity(connection, row.7)?,
        });
    }
    Ok(found)
}

/// What the semantic tier has to say about one Resource's coverage.
///
/// Core's job, not a caller's: which contexts contribute, whether any of
/// them is stale, and how many sites the two tiers disagree about.
pub fn semantic_scope(
    connection: &Connection,
    resource: ResourceId,
) -> Result<SemanticScope, MergeError> {
    let uid = resource.to_bytes().to_vec();
    let conflicts: i64 = connection.query_row(
        "SELECT COUNT(*) FROM semantic_conflict c \
         JOIN occurrence o ON o.id = c.occurrence_id \
         JOIN resource r ON r.id = o.resource_id WHERE r.uid = ?1",
        params![uid.clone()],
        |row| row.get(0),
    )?;

    let mut statement = connection.prepare(
        "SELECT DISTINCT e.context_key FROM semantic_evidence e \
         JOIN occurrence o ON o.id = e.occurrence_id \
         JOIN resource r ON r.id = o.resource_id WHERE r.uid = ?1 \
         UNION \
         SELECT DISTINCT c.context_key FROM semantic_conflict c \
         JOIN occurrence o2 ON o2.id = c.occurrence_id \
         JOIN resource r2 ON r2.id = o2.resource_id WHERE r2.uid = ?1 \
         ORDER BY 1",
    )?;
    let contexts: Vec<String> = statement
        .query_map(params![uid], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    drop(statement);

    let mut not_current = false;
    for context_key in &contexts {
        let state: Option<String> = connection
            .query_row(
                "SELECT detail_state FROM component_state \
                 WHERE component_kind = 'SEMANTIC_INDEX' AND scope_kind = 'ANALYSIS_CONTEXT' \
                   AND scope_key = ?1",
                params![context_key],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        // A contributing context with no component row at all has never
        // published: that is not current either.
        if state.as_deref() != Some(SemanticState::Current.as_str()) {
            not_current = true;
        }
    }

    Ok(SemanticScope {
        contexts: contexts.len(),
        conflicts: usize::try_from(conflicts).unwrap_or(0),
        not_current,
    })
}

// ---------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------

/// The three things one piece of evidence can turn into.
///
/// Boxed where it is large: a conflict carries three endpoints, and an
/// apply carries a whole relation.
enum Planned {
    Apply(Box<PlannedApply>),
    Conflict {
        occurrence_id: i64,
        conflict: Box<SemanticConflict>,
    },
    Rejected(RejectedEvidence),
}

struct PlannedApply {
    occurrence_id: i64,
    relation: Relation,
    role: ProofRole,
    capability: SemanticCapability,
}

fn plan(
    connection: &Connection,
    context_key: &str,
    owner: i64,
    revision: &str,
    item: &SemanticEvidence,
) -> Result<Planned, MergeError> {
    let reject = |reason| {
        Ok(Planned::Rejected(RejectedEvidence {
            capability: item.capability,
            occurrence: item.occurrence,
            reason,
        }))
    };

    let Some(occurrence) = item.occurrence else {
        return reject(RejectionReason::NoOccurrence);
    };
    let SemanticOutcome::Resolved { target } = &item.outcome else {
        // A candidate set is not a proof, and neither is an unresolved
        // site. Promotion on count alone is the guess this tier exists
        // to refuse.
        return reject(RejectionReason::NotResolved);
    };
    let Some(kind) = item.relation_kind else {
        return reject(RejectionReason::NoRelationKind);
    };
    let Some(source) = item.source.clone() else {
        return reject(RejectionReason::NoSource);
    };

    let Some((occurrence_id, occurrence_revision, bound)) =
        occurrence_row(connection, owner, occurrence)?
    else {
        // No structural Occurrence at that span. Nothing is invented to
        // hold the edge: an unanchored relation is a claim about a site
        // nobody can read.
        return reject(RejectionReason::UnanchoredOccurrence);
    };
    if occurrence_revision != revision {
        return reject(RejectionReason::StaleOccurrence);
    }

    // `created_generation` is filled in by the caller, which holds the
    // publication grant; it takes no part in canonical identity.
    let relation = Relation {
        kind,
        source: source.clone(),
        target: target.clone(),
        dispatch: item.dispatch,
        created_generation: 0,
    };

    // A binding this same context established last time is this
    // context's own previous answer, not the structural tier's claim.
    // Replacing it is a revalidation; calling it a conflict would make a
    // backend disagree with itself.
    let bound = match bound {
        Some(id) if own_binding(connection, context_key, occurrence_id)? => {
            let _ = id;
            None
        }
        other => other,
    };

    match bound {
        None => Ok(Planned::Apply(Box::new(PlannedApply {
            occurrence_id,
            relation,
            role: ProofRole::Binding,
            capability: item.capability,
        }))),
        Some(bound_id) => {
            let existing = relation_by_row(connection, bound_id)?;
            if existing.kind != kind || existing.source != source {
                return reject(RejectionReason::OccurrenceBoundElsewhere);
            }
            if existing.target == *target {
                // The same edge, proved twice. One relation, one
                // occurrence, combined provenance.
                return Ok(Planned::Apply(Box::new(PlannedApply {
                    occurrence_id,
                    relation,
                    role: ProofRole::Corroborating,
                    capability: item.capability,
                })));
            }
            Ok(Planned::Conflict {
                occurrence_id,
                conflict: Box::new(SemanticConflict {
                    context_key: item.context_key.clone(),
                    occurrence,
                    kind,
                    source,
                    structural_target: existing.target,
                    semantic_target: target.clone(),
                }),
            })
        }
    }
}

// ---------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------

/// Whether this context's own previous binding proof holds this
/// Occurrence.
fn own_binding(
    connection: &Connection,
    context_key: &str,
    occurrence_id: i64,
) -> Result<bool, MergeError> {
    let role: Option<String> = connection
        .query_row(
            "SELECT proof_role FROM semantic_evidence \
             WHERE context_key = ?1 AND occurrence_id = ?2",
            params![context_key, occurrence_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(role.as_deref() == Some(ProofRole::Binding.as_str()))
}

struct Contribution {
    relation_id: i64,
    role: ProofRole,
    displaced: Option<DisplacedGap>,
}

fn load_contribution(
    connection: &Connection,
    context_key: &str,
    owner: i64,
) -> Result<BTreeMap<i64, Contribution>, MergeError> {
    load_contribution_opt(connection, context_key, Some(owner))
}

fn load_contribution_opt(
    connection: &Connection,
    context_key: &str,
    owner: Option<i64>,
) -> Result<BTreeMap<i64, Contribution>, MergeError> {
    let mut statement = connection.prepare(
        "SELECT e.occurrence_id, e.relation_id, e.proof_role, e.displaced_intended_kind, \
                e.displaced_lookup_name, e.displaced_module_hint, e.displaced_reason \
         FROM semantic_evidence e \
         JOIN occurrence o ON o.id = e.occurrence_id \
         WHERE e.context_key = ?1 AND (?2 IS NULL OR o.resource_id = ?2) \
         ORDER BY e.occurrence_id",
    )?;
    let rows: Vec<RawContributionRow> = statement
        .query_map(params![context_key, owner], |row| {
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
    drop(statement);

    let mut found = BTreeMap::new();
    for row in rows {
        let displaced = match (row.3, row.4, row.6) {
            (Some(intended), Some(lookup_name), Some(reason)) => Some(DisplacedGap {
                intended: IntendedRelation::parse(&intended)?,
                lookup_name,
                module_hint: row.5,
                reason: UnresolvedReason::parse(&reason)?,
            }),
            _ => None,
        };
        found.insert(
            row.0,
            Contribution {
                relation_id: row.1,
                role: ProofRole::parse(&row.2)?,
                displaced,
            },
        );
    }
    Ok(found)
}

/// Remove one entry of a context's contribution, restoring whatever gap
/// it displaced. Returns whether a gap came back.
fn withdraw_one(
    connection: &Connection,
    context_key: &str,
    occurrence_id: i64,
    entry: &Contribution,
) -> Result<bool, MergeError> {
    connection.execute(
        "DELETE FROM semantic_evidence WHERE context_key = ?1 AND occurrence_id = ?2",
        params![context_key, occurrence_id],
    )?;
    if entry.role == ProofRole::Corroborating {
        // Corroboration leaving costs the structural edge nothing.
        return Ok(false);
    }

    // Another context may still bind this same occurrence; only the last
    // binding proof unbinds it.
    let still_bound: i64 = connection.query_row(
        "SELECT COUNT(*) FROM semantic_evidence \
         WHERE occurrence_id = ?1 AND proof_role = ?2",
        params![occurrence_id, ProofRole::Binding.as_str()],
        |row| row.get(0),
    )?;
    if still_bound > 0 {
        return Ok(false);
    }

    connection.execute(
        "UPDATE occurrence SET relation_id = NULL WHERE id = ?1",
        params![occurrence_id],
    )?;
    let Some(gap) = &entry.displaced else {
        return Ok(false);
    };
    connection.execute(
        "INSERT INTO unresolved_reference \
         (occurrence_id, intended_relation_kind, lookup_name, module_hint, reason, \
          resolution_context_id, candidate_truncated) \
         VALUES (?1, ?2, ?3, ?4, ?5, NULL, 0) \
         ON CONFLICT (occurrence_id) DO NOTHING",
        params![
            occurrence_id,
            gap.intended.as_str(),
            gap.lookup_name,
            gap.module_hint,
            gap.reason.as_str(),
        ],
    )?;
    Ok(true)
}

/// Take the structural gap off an Occurrence that semantic evidence is
/// about to resolve, returning what it said.
fn take_gap(
    connection: &Connection,
    occurrence_id: i64,
) -> Result<Option<DisplacedGap>, MergeError> {
    let row: Option<(i64, String, String, Option<String>, String)> = connection
        .query_row(
            "SELECT id, intended_relation_kind, lookup_name, module_hint, reason \
             FROM unresolved_reference WHERE occurrence_id = ?1",
            params![occurrence_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(None);
    };
    // Candidates hang off the unresolved row and go with it: a resolved
    // site has no candidates to choose between any more.
    connection.execute(
        "DELETE FROM relation_candidate WHERE unresolved_reference_id = ?1",
        params![row.0],
    )?;
    connection.execute(
        "DELETE FROM unresolved_reference WHERE id = ?1",
        params![row.0],
    )?;
    Ok(Some(DisplacedGap {
        intended: IntendedRelation::parse(&row.1)?,
        lookup_name: row.2,
        module_hint: row.3,
        reason: UnresolvedReason::parse(&row.4)?,
    }))
}

#[allow(clippy::too_many_arguments)]
fn upsert_semantic_evidence(
    connection: &Connection,
    context_key: &str,
    occurrence_id: i64,
    relation_id: i64,
    capability: SemanticCapability,
    role: ProofRole,
    displaced: Option<&DisplacedGap>,
    analysis_profile_id: i64,
    grant: &PublicationGrant,
) -> Result<(), MergeError> {
    connection.execute(
        "INSERT INTO semantic_evidence \
         (context_key, occurrence_id, relation_id, capability, proof_role, \
          analysis_profile_id, generation_id, displaced_intended_kind, \
          displaced_lookup_name, displaced_module_hint, displaced_reason) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
         ON CONFLICT (context_key, occurrence_id) DO UPDATE SET \
         relation_id = excluded.relation_id, \
         capability = excluded.capability, \
         proof_role = excluded.proof_role, \
         analysis_profile_id = excluded.analysis_profile_id, \
         generation_id = excluded.generation_id, \
         displaced_intended_kind = excluded.displaced_intended_kind, \
         displaced_lookup_name = excluded.displaced_lookup_name, \
         displaced_module_hint = excluded.displaced_module_hint, \
         displaced_reason = excluded.displaced_reason",
        params![
            context_key,
            occurrence_id,
            relation_id,
            capability.as_str(),
            role.as_str(),
            analysis_profile_id,
            grant.generation_id(),
            displaced.map(|gap| gap.intended.as_str()),
            displaced.map(|gap| gap.lookup_name.clone()),
            displaced.and_then(|gap| gap.module_hint.clone()),
            displaced.map(|gap| gap.reason.as_str()),
        ],
    )?;
    Ok(())
}

/// Record a disagreement.
///
/// The semantic target's entity is ensured so the conflict names it in
/// canonical identity -- but no `relation` row is written for it, which
/// is the difference between recording a disagreement and picking a
/// side.
fn write_conflict(
    connection: &Connection,
    occurrence_id: i64,
    conflict: &SemanticConflict,
    analysis_profile_id: i64,
    grant: &PublicationGrant,
) -> Result<(), MergeError> {
    let mut entity_ids = Vec::with_capacity(3);
    for endpoint in [
        &conflict.source,
        &conflict.structural_target,
        &conflict.semantic_target,
    ] {
        graph::ensure_entity(connection, endpoint)?;
        entity_ids.push(graph::require_entity_id(connection, endpoint)?);
    }
    connection.execute(
        "INSERT INTO semantic_conflict \
         (context_key, occurrence_id, relation_kind, source_entity_id, \
          structural_target_entity_id, semantic_target_entity_id, \
          analysis_profile_id, generation_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT (context_key, occurrence_id, relation_kind) DO UPDATE SET \
         source_entity_id = excluded.source_entity_id, \
         structural_target_entity_id = excluded.structural_target_entity_id, \
         semantic_target_entity_id = excluded.semantic_target_entity_id, \
         analysis_profile_id = excluded.analysis_profile_id, \
         generation_id = excluded.generation_id",
        params![
            conflict.context_key,
            occurrence_id,
            conflict.kind.as_str(),
            entity_ids[0],
            entity_ids[1],
            entity_ids[2],
            analysis_profile_id,
            grant.generation_id(),
        ],
    )?;
    Ok(())
}

fn clear_conflicts(
    connection: &Connection,
    context_key: &str,
    owner: i64,
) -> Result<(), MergeError> {
    connection.execute(
        "DELETE FROM semantic_conflict WHERE context_key = ?1 AND occurrence_id IN \
         (SELECT id FROM occurrence WHERE resource_id = ?2)",
        params![context_key, owner],
    )?;
    Ok(())
}

fn bind_occurrence(
    connection: &Connection,
    occurrence_id: i64,
    relation_id: i64,
) -> Result<(), MergeError> {
    connection.execute(
        "UPDATE occurrence SET relation_id = ?1 WHERE id = ?2",
        params![relation_id, occurrence_id],
    )?;
    Ok(())
}

/// Remove every relation among `candidates` that nothing proves any
/// more.
fn collect_unevidenced(connection: &Connection, candidates: &[i64]) -> Result<usize, MergeError> {
    let mut removed = 0;
    let mut seen: Vec<i64> = Vec::new();
    for relation_id in candidates {
        if seen.contains(relation_id) {
            continue;
        }
        seen.push(*relation_id);
        let changed = connection.execute(
            "DELETE FROM relation WHERE id = ?1 \
             AND NOT EXISTS (SELECT 1 FROM occurrence WHERE relation_id = ?1)",
            params![relation_id],
        )?;
        removed += changed;
    }
    Ok(removed)
}

fn owner_row(connection: &Connection, resource: ResourceId) -> Result<i64, MergeError> {
    connection
        .query_row(
            "SELECT id FROM resource WHERE uid = ?1 AND state = 'ACTIVE'",
            params![resource.to_bytes().to_vec()],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(MergeError::UnknownResource { resource })
}

fn owner_revision(connection: &Connection, owner: i64) -> Result<String, MergeError> {
    Ok(connection.query_row(
        "SELECT resource_revision FROM resource WHERE id = ?1",
        params![owner],
        |row| row.get(0),
    )?)
}

/// The Occurrence at one exact span, its revision, and what it binds.
fn occurrence_row(
    connection: &Connection,
    owner: i64,
    occurrence: OccurrenceRef,
) -> Result<Option<(i64, String, Option<i64>)>, MergeError> {
    Ok(connection
        .query_row(
            "SELECT id, resource_revision, relation_id FROM occurrence \
             WHERE resource_id = ?1 AND kind = ?2 AND start_byte = ?3 AND end_byte = ?4",
            params![
                owner,
                occurrence.kind.to_string(),
                i64::try_from(occurrence.start_byte).unwrap_or(i64::MAX),
                i64::try_from(occurrence.end_byte).unwrap_or(i64::MAX),
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?)
}

fn relation_row(connection: &Connection, relation: &Relation) -> Result<Option<i64>, MergeError> {
    Ok(graph::relation_row_id(connection, &relation.key())?)
}

/// The row id of an edge this call has just ensured exists.
fn require_relation_row(connection: &Connection, relation: &Relation) -> Result<i64, MergeError> {
    relation_row(connection, relation)?
        .ok_or(MergeError::Sqlite(rusqlite::Error::QueryReturnedNoRows))
}

fn relation_by_row(connection: &Connection, relation_id: i64) -> Result<Relation, MergeError> {
    let row: (String, i64, i64, String, i64) = connection.query_row(
        "SELECT kind, source_entity_id, target_entity_id, dispatch, created_generation \
         FROM relation WHERE id = ?1",
        params![relation_id],
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
    Ok(Relation {
        kind: RelationKind::parse(&row.0)?,
        source: graph::endpoint_of_entity(connection, row.1)?,
        target: graph::endpoint_of_entity(connection, row.2)?,
        dispatch: crate::resolution::Dispatch::parse(&row.3)
            .map_err(GraphError::UnknownAxisValue)?,
        created_generation: row.4,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use brainprint_core::{SymbolId, WorkspaceId};

    use super::*;
    use crate::{
        config::WorkspaceConfig,
        coverage::CoverageLimit,
        evidence::{RelationEvidence, replace_resource_graph},
        gaps::UnresolvedEvidence,
        generation,
        graph::{ExternalEntity, GraphStore},
        impact::{Budget, ImpactIntent, ImpactTraversal},
        prepare::InspectPreparer,
        related_tests::RelatedTests,
        relations::{Direction, RelationIndex},
        resolution::{Dispatch, EvidenceBasis, Support},
        resource::{Resource, ResourceLanguage, ResourceStore},
        scan::BaselineScan,
        semantic::{
            AnalysisContext, ProjectRootIdentity, SemanticCapability, SemanticEvidence,
            SemanticOutcome, ToolchainIdentity,
        },
        semantic_index::{SemanticBasis, SemanticStatus},
        symbol::SymbolStore,
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    const APP_TS: &str = "\
import { save } from './store'

export function run(): void {
  save()
  save()
}

export function wired(): void {
  handler()
}

export function guessed(): void {
  maybe()
}
";

    const STORE_TS: &str = "\
export function save(): void {}
export function persist(): void {}
export function handler(): void {}
export function maybe(): void {}
";

    const OTHER_TS: &str = "\
import { persist } from './store'

export function keep(): void {
  persist()
}
";

    const APP_TEST_TS: &str = "\
import { wired } from './app'

export function testsWired(): void {
  wired()
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
                "brainprint-merge-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            fs::create_dir_all(root.join("src")).expect("src");
            let fixture = Self { base, root };
            fixture.write("src/app.ts", APP_TS);
            fixture.write("src/store.ts", STORE_TS);
            fixture.write("src/other.ts", OTHER_TS);
            fixture.write("src/app.test.ts", APP_TEST_TS);
            BaselineScan::open(&fixture.db_path())
                .expect("index.db")
                .run_initial_scan(
                    &fixture.root,
                    &WorkspaceConfig::default(),
                    "workspace-rev-1",
                )
                .expect("baseline scan");
            fixture.publish_structural();
            fixture
        }

        fn db_path(&self) -> PathBuf {
            self.base.join("data").join("index.db")
        }

        fn write(&self, rel: &str, contents: &str) {
            fs::write(self.root.join(rel), contents).expect("fixture file");
        }

        fn resource(&self, rel: &str) -> Resource {
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

        fn store(&self) -> GraphStore {
            GraphStore::open(&self.db_path()).expect("index.db")
        }

        fn basis(&self, rel: &str, generation_id: i64) -> EvidenceBasis {
            let resource = self.resource(rel);
            let profile_id = self.profile_id(rel);
            EvidenceBasis {
                owner_resource: resource.id,
                owner_resource_revision: resource.resource_revision,
                generation_id,
                analysis_profile_id: profile_id,
                resolution_context_key: None,
            }
        }

        fn profile_id(&self, rel: &str) -> i64 {
            SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_for_resource(self.resource(rel).id)
                .expect("symbols")
                .first()
                .expect("the file declares something")
                .analysis_profile_id
        }

        /// The I3 baseline: two call sites of one confirmed edge, one
        /// unresolved site, one site with candidates, and an unrelated
        /// Resource's own confirmed edge.
        fn publish_structural(&self) {
            let calls = self.sites("src/app.ts", OccurrenceKind::CallSite);
            assert_eq!(calls.len(), 4, "save, save, handler, maybe");
            let other_calls = self.sites("src/other.ts", OccurrenceKind::CallSite);
            assert_eq!(other_calls.len(), 1);
            let test_calls = self.sites("src/app.test.ts", OccurrenceKind::CallSite);
            assert_eq!(test_calls.len(), 1);

            let run = GraphEndpoint::Symbol(self.symbol("src/app.ts", "run"));
            let wired = GraphEndpoint::Symbol(self.symbol("src/app.ts", "wired"));
            let guessed = GraphEndpoint::Symbol(self.symbol("src/app.ts", "guessed"));
            let keep = GraphEndpoint::Symbol(self.symbol("src/other.ts", "keep"));
            let save = GraphEndpoint::Symbol(self.symbol("src/store.ts", "save"));
            let persist = GraphEndpoint::Symbol(self.symbol("src/store.ts", "persist"));
            let handler = GraphEndpoint::Symbol(self.symbol("src/store.ts", "handler"));
            let maybe = GraphEndpoint::Symbol(self.symbol("src/store.ts", "maybe"));
            let _ = &handler;

            let store = self.store();
            let connection = store.connection();
            let revision = generation::current_workspace_revision(connection)
                .expect("clock")
                .expect("bootstrapped");
            let building = generation::begin_generation(connection, &revision).expect("begin");
            let transaction = connection.unchecked_transaction().expect("transaction");
            let (record, grant) =
                generation::grant_publication(&transaction, building.id).expect("grant");

            let app_resolved = vec![
                RelationEvidence {
                    occurrence: calls[0],
                    relation: Relation {
                        kind: RelationKind::Calls,
                        source: run.clone(),
                        target: save.clone(),
                        dispatch: Dispatch::Static,
                        created_generation: building.id,
                    },
                },
                RelationEvidence {
                    occurrence: calls[1],
                    relation: Relation {
                        kind: RelationKind::Calls,
                        source: run.clone(),
                        target: save.clone(),
                        dispatch: Dispatch::Static,
                        created_generation: building.id,
                    },
                },
            ];
            let app_gaps = vec![
                UnresolvedEvidence {
                    occurrence: calls[2],
                    intended: IntendedRelation::Known(RelationKind::Calls),
                    lookup_name: "handler".to_owned(),
                    module_hint: None,
                    reason: UnresolvedReason::ReceiverTypeRequired,
                    candidates: Vec::new(),
                },
                UnresolvedEvidence {
                    occurrence: calls[3],
                    intended: IntendedRelation::Known(RelationKind::Calls),
                    lookup_name: "maybe".to_owned(),
                    module_hint: None,
                    reason: UnresolvedReason::AmbiguousCandidates,
                    candidates: vec![maybe.clone(), persist.clone()],
                },
            ];
            let tests_wired = GraphEndpoint::Symbol(self.symbol("src/app.test.ts", "testsWired"));
            let test_resolved = vec![RelationEvidence {
                occurrence: test_calls[0],
                relation: Relation {
                    kind: RelationKind::Calls,
                    source: tests_wired,
                    target: wired.clone(),
                    dispatch: Dispatch::Static,
                    created_generation: building.id,
                },
            }];
            let other_resolved = vec![RelationEvidence {
                occurrence: other_calls[0],
                relation: Relation {
                    kind: RelationKind::Calls,
                    source: keep.clone(),
                    target: persist.clone(),
                    dispatch: Dispatch::Static,
                    created_generation: building.id,
                },
            }];

            for item in app_resolved
                .iter()
                .chain(other_resolved.iter())
                .chain(test_resolved.iter())
            {
                for endpoint in [&item.relation.source, &item.relation.target] {
                    graph::ensure_entity(&transaction, endpoint).expect("ensure");
                }
            }
            for gap in &app_gaps {
                for candidate in &gap.candidates {
                    graph::ensure_entity(&transaction, candidate).expect("ensure");
                }
            }
            let _ = &guessed;

            replace_resource_graph(
                &transaction,
                &grant,
                &self.basis("src/app.ts", building.id),
                &app_resolved,
                &app_gaps,
            )
            .expect("app evidence");
            replace_resource_graph(
                &transaction,
                &grant,
                &self.basis("src/other.ts", building.id),
                &other_resolved,
                &[],
            )
            .expect("other evidence");
            replace_resource_graph(
                &transaction,
                &grant,
                &self.basis("src/app.test.ts", building.id),
                &test_resolved,
                &[],
            )
            .expect("test evidence");
            generation::finish_publish_stable(&transaction, &record).expect("stable");
            transaction.commit().expect("commit");
        }

        /// Run one semantic merge inside a real publication.
        fn merge(&self, request: &dyn Fn(i64) -> Request) -> Result<MergeOutcome, MergeError> {
            let store = self.store();
            let connection = store.connection();
            let revision = generation::current_workspace_revision(connection)
                .expect("clock")
                .expect("bootstrapped");
            let building = generation::begin_generation(connection, &revision).expect("begin");
            let transaction = connection.unchecked_transaction().expect("transaction");
            let (record, grant) =
                generation::grant_publication(&transaction, building.id).expect("grant");
            let built = request(building.id);
            let outcome = merge(
                &transaction,
                &grant,
                &MergeRequest {
                    context: &built.context,
                    status: &built.status,
                    owner: built.owner,
                    analysis_profile_id: built.profile_id,
                    evidence: &built.evidence,
                },
            );
            match outcome {
                Ok(outcome) => {
                    generation::finish_publish_stable(&transaction, &record).expect("stable");
                    transaction.commit().expect("commit");
                    Ok(outcome)
                }
                Err(error) => {
                    // A refused merge writes nothing: the whole
                    // transaction goes.
                    drop(transaction);
                    generation::abort_generation(connection, building.id, "refused")
                        .expect("abort");
                    Err(error)
                }
            }
        }

        fn withdraw(&self, context_key: &str) -> MergeOutcome {
            let store = self.store();
            let connection = store.connection();
            let transaction = connection.unchecked_transaction().expect("transaction");
            let outcome = withdraw(&transaction, context_key, None).expect("withdraw");
            transaction.commit().expect("commit");
            outcome
        }

        fn counts(&self) -> (i64, i64, i64, i64) {
            let store = self.store();
            let connection = store.connection();
            let count = |sql: &str| -> i64 {
                connection
                    .query_row(sql, [], |row| row.get(0))
                    .expect("count")
            };
            (
                count("SELECT COUNT(*) FROM relation"),
                count("SELECT COUNT(*) FROM occurrence WHERE relation_id IS NOT NULL"),
                count("SELECT COUNT(*) FROM unresolved_reference"),
                count("SELECT COUNT(*) FROM semantic_evidence"),
            )
        }

        fn callers_of(&self, target: &GraphEndpoint) -> Vec<GraphEndpoint> {
            RelationIndex::open(&self.db_path())
                .expect("index.db")
                .callers(target)
                .expect("query")
                .confirmed
                .into_iter()
                .map(|result| result.source)
                .collect()
        }

        fn outgoing(&self, source: &GraphEndpoint) -> crate::relations::RelationAnswer {
            RelationIndex::open(&self.db_path())
                .expect("index.db")
                .outgoing(source, &[])
                .expect("query")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// Everything one merge call needs, built against a live generation.
    struct Request {
        context: AnalysisContext,
        status: SemanticStatus,
        owner: ResourceId,
        profile_id: i64,
        evidence: Vec<SemanticEvidence>,
    }

    fn context() -> AnalysisContext {
        AnalysisContext {
            workspace: WorkspaceId::from_bytes([1; 16]),
            backend: crate::semantic::SemanticBackendKind::TypeScriptJavaScript,
            language: ResourceLanguage::TypeScript,
            project_root: ProjectRootIdentity::Key("workspace".to_owned()),
            toolchain: ToolchainIdentity {
                backend_version: "1.0.0".to_owned(),
                backend_compatibility_class: "fake-ts:1".to_owned(),
                environment_fingerprint: "sha256:node".to_owned(),
            },
        }
    }

    fn other_context() -> AnalysisContext {
        let mut other = context();
        other.project_root = ProjectRootIdentity::Key("elsewhere".to_owned());
        other
    }

    /// The same project in a different worktree: a different
    /// `WorkspaceId`, so a different context entirely.
    fn other_worktree() -> AnalysisContext {
        let mut other = context();
        other.workspace = WorkspaceId::from_bytes([9; 16]);
        other
    }

    fn current_status(context: &AnalysisContext) -> SemanticStatus {
        SemanticStatus {
            state: SemanticState::Current,
            support: Some(Support::Supported),
            stable_generation_id: Some(1),
            basis: Some(SemanticBasis::new(
                context,
                &crate::semantic_index::ConfigBasis::new(),
            )),
            last_error_code: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn evidence(
        fixture: &Fixture,
        context: &AnalysisContext,
        rel: &str,
        occurrence: Option<OccurrenceRef>,
        capability: SemanticCapability,
        kind: Option<RelationKind>,
        source: Option<GraphEndpoint>,
        outcome: SemanticOutcome,
        generation_id: i64,
    ) -> SemanticEvidence {
        SemanticEvidence {
            context_key: context.context_key(),
            capability,
            relation_kind: kind,
            basis: fixture.basis(rel, generation_id),
            occurrence,
            source,
            outcome,
            support: Support::Supported,
            dispatch: Dispatch::Static,
        }
    }

    /// The common case: one resolved CALLS at one site of `src/app.ts`.
    fn resolved_call(
        fixture: &Fixture,
        context: &AnalysisContext,
        site: usize,
        source: GraphEndpoint,
        target: GraphEndpoint,
        generation_id: i64,
    ) -> SemanticEvidence {
        let sites = fixture.sites("src/app.ts", OccurrenceKind::CallSite);
        evidence(
            fixture,
            context,
            "src/app.ts",
            Some(sites[site]),
            SemanticCapability::CallsCrossFile,
            Some(RelationKind::Calls),
            Some(source),
            SemanticOutcome::Resolved { target },
            generation_id,
        )
    }

    fn app_request(fixture: &Fixture, evidence: Vec<SemanticEvidence>) -> Request {
        Request {
            context: context(),
            status: current_status(&context()),
            owner: fixture.resource("src/app.ts").id,
            profile_id: fixture.profile_id("src/app.ts"),
            evidence,
        }
    }

    // -----------------------------------------------------------------
    // Gap resolution and corroboration
    // -----------------------------------------------------------------

    #[test]
    fn semantic_evidence_resolves_a_structural_gap_on_its_own_occurrence() {
        let fixture = Fixture::create("gap-resolved");
        let wired = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "wired"));
        let handler = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "handler"));
        let site = fixture.sites("src/app.ts", OccurrenceKind::CallSite)[2];

        let before = fixture.counts();
        let outcome = fixture
            .merge(&|generation| {
                app_request(
                    &fixture,
                    vec![resolved_call(
                        &fixture,
                        &context(),
                        2,
                        wired.clone(),
                        handler.clone(),
                        generation,
                    )],
                )
            })
            .expect("merge");

        assert_eq!(outcome.gaps_resolved, 1);
        assert_eq!(outcome.relations_created, 1);
        assert_eq!(outcome.conflicts, 0);
        assert!(outcome.rejected.is_empty());

        let after = fixture.counts();
        assert_eq!(after.0, before.0 + 1, "exactly one new canonical edge");
        assert_eq!(after.1, before.1 + 1, "one more bound occurrence");
        assert_eq!(after.2, before.2 - 1, "the gap is gone, not duplicated");

        // The resolved edge is anchored to the very Occurrence that was
        // the gap -- no new site was invented.
        let store = fixture.store();
        let bound: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM occurrence o \
                 JOIN semantic_evidence e ON e.occurrence_id = o.id \
                 WHERE o.start_byte = ?1 AND o.end_byte = ?2 AND o.relation_id IS NOT NULL",
                params![site.start_byte as i64, site.end_byte as i64],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(bound, 1);
        assert_eq!(fixture.callers_of(&handler), vec![wired]);
    }

    #[test]
    fn a_candidate_set_is_resolved_by_proof_and_never_by_count() {
        let fixture = Fixture::create("candidate");
        let guessed = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "guessed"));
        let maybe = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "maybe"));

        // Before any semantic proof the site stays a gap, candidates and
        // all. Two candidates, one of which is "obviously" right, is
        // still a guess.
        let answer = fixture.outgoing(&guessed);
        assert!(answer.confirmed.is_empty());
        assert_eq!(answer.gaps.len(), 1);
        assert!(!answer.gaps[0].candidates.is_empty());

        let outcome = fixture
            .merge(&|generation| {
                app_request(
                    &fixture,
                    vec![resolved_call(
                        &fixture,
                        &context(),
                        3,
                        guessed.clone(),
                        maybe.clone(),
                        generation,
                    )],
                )
            })
            .expect("merge");
        assert_eq!(outcome.gaps_resolved, 1);

        let answer = fixture.outgoing(&guessed);
        assert_eq!(answer.confirmed.len(), 1);
        assert_eq!(answer.confirmed[0].target, maybe);
        assert!(answer.gaps.is_empty(), "the candidate set went with it");
        let candidates: i64 = fixture
            .store()
            .connection()
            .query_row("SELECT COUNT(*) FROM relation_candidate", [], |row| {
                row.get(0)
            })
            .expect("count");
        assert_eq!(candidates, 0);
    }

    #[test]
    fn a_confirming_backend_adds_provenance_and_not_a_second_edge() {
        let fixture = Fixture::create("corroborate");
        let run = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"));
        let save = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "save"));

        let before = fixture.counts();
        let outcome = fixture
            .merge(&|generation| {
                app_request(
                    &fixture,
                    vec![
                        resolved_call(
                            &fixture,
                            &context(),
                            0,
                            run.clone(),
                            save.clone(),
                            generation,
                        ),
                        resolved_call(
                            &fixture,
                            &context(),
                            1,
                            run.clone(),
                            save.clone(),
                            generation,
                        ),
                    ],
                )
            })
            .expect("merge");

        assert_eq!(outcome.corroborated, 2);
        assert_eq!(outcome.relations_created, 0);
        assert_eq!(outcome.gaps_resolved, 0);

        let after = fixture.counts();
        assert_eq!(after.0, before.0, "no second CALLS row");
        assert_eq!(after.1, before.1, "no extra bound occurrence");
        assert_eq!(after.3, 2, "two sites, two pieces of semantic provenance");

        // Two call sites of one function stay two occurrences of one
        // edge -- exactly as they were before the backend spoke.
        let answer = fixture.outgoing(&run);
        let calls: Vec<_> = answer
            .confirmed
            .iter()
            .filter(|result| result.kind == RelationKind::Calls)
            .collect();
        assert_eq!(calls.len(), 1, "one canonical relation");
        assert_eq!(calls[0].evidence.len(), 2, "two evidence sites");
    }

    #[test]
    fn provenance_is_identity_and_a_capability_name_not_a_backend_response() {
        let fixture = Fixture::create("provenance");
        let wired = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "wired"));
        let handler = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "handler"));
        fixture
            .merge(&|generation| {
                app_request(
                    &fixture,
                    vec![resolved_call(
                        &fixture,
                        &context(),
                        2,
                        wired.clone(),
                        handler.clone(),
                        generation,
                    )],
                )
            })
            .expect("merge");

        let store = fixture.store();
        let row: (String, String, String, i64) = store
            .connection()
            .query_row(
                "SELECT context_key, capability, proof_role, analysis_profile_id \
                 FROM semantic_evidence",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("row");
        assert_eq!(row.0, context().context_key());
        assert_eq!(row.1, SemanticCapability::CallsCrossFile.as_str());
        assert_eq!(row.2, ProofRole::Binding.as_str());
        assert!(row.3 > 0, "a profile reference, not a version tuple");

        // Nothing resembling a backend's own identity or output.
        let rendered = format!("{row:?}");
        for forbidden in ["RequestId", "pyright", "textDocument", "export function"] {
            assert!(
                !rendered.contains(forbidden),
                "{forbidden} must not persist"
            );
        }
    }

    // -----------------------------------------------------------------
    // Conflict
    // -----------------------------------------------------------------

    #[test]
    fn two_tiers_confirming_different_targets_is_a_conflict_and_not_a_vote() {
        let fixture = Fixture::create("conflict");
        let run = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"));
        let save = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "save"));
        let persist = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "persist"));

        let outcome = fixture
            .merge(&|generation| {
                app_request(
                    &fixture,
                    vec![resolved_call(
                        &fixture,
                        &context(),
                        0,
                        run.clone(),
                        persist.clone(),
                        generation,
                    )],
                )
            })
            .expect("merge");
        assert_eq!(outcome.conflicts, 1);
        assert_eq!(outcome.relations_created, 0, "semantic did not win");
        assert_eq!(outcome.corroborated, 0);

        // Structural truth is still there, and untouched.
        let answer = fixture.outgoing(&run);
        let targets: Vec<_> = answer
            .confirmed
            .iter()
            .filter(|result| result.kind == RelationKind::Calls)
            .map(|result| result.target.clone())
            .collect();
        assert_eq!(targets, vec![save.clone()], "structural was not deleted");
        assert!(
            !targets.contains(&persist),
            "the semantic target did not become a confirmed edge"
        );

        // Both are retained, and the scope is no longer a clean answer.
        let conflicts = conflicts_for_resource(
            fixture.store().connection(),
            fixture.resource("src/app.ts").id,
        )
        .expect("conflicts");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind, RelationKind::Calls);
        assert_eq!(conflicts[0].source, run);
        assert_eq!(conflicts[0].structural_target, save);
        assert_eq!(conflicts[0].semantic_target, persist);

        assert_eq!(answer.coverage.semantic.conflicts, 1);
        assert!(
            answer
                .coverage
                .limits()
                .has(CoverageLimit::SemanticConflict)
        );
        assert!(
            !answer.coverage.is_complete(),
            "not a clean complete answer"
        );
    }

    // -----------------------------------------------------------------
    // Semantic-only relations and anchoring
    // -----------------------------------------------------------------

    #[test]
    fn a_semantic_only_relation_needs_an_exact_current_source_site() {
        let fixture = Fixture::create("semantic-only");
        let wired = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "wired"));
        let handler = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "handler"));
        let site = fixture.sites("src/app.ts", OccurrenceKind::CallSite)[2];

        // An OVERRIDES the structural tier could never prove, anchored to
        // a real site: allowed.
        let outcome = fixture
            .merge(&|generation| {
                app_request(
                    &fixture,
                    vec![evidence(
                        &fixture,
                        &context(),
                        "src/app.ts",
                        Some(site),
                        SemanticCapability::Overrides,
                        Some(RelationKind::Overrides),
                        Some(wired.clone()),
                        SemanticOutcome::Resolved {
                            target: handler.clone(),
                        },
                        generation,
                    )],
                )
            })
            .expect("merge");
        assert_eq!(outcome.relations_created, 1);

        let answer = fixture.outgoing(&wired);
        assert!(
            answer
                .confirmed
                .iter()
                .any(|result| result.kind == RelationKind::Overrides),
            "a relation only a backend can prove is canonical like any other"
        );
    }

    #[test]
    fn evidence_that_cannot_be_anchored_is_rejected_rather_than_invented() {
        let fixture = Fixture::create("unanchored");
        let wired = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "wired"));
        let handler = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "handler"));
        let before = fixture.counts();

        let nowhere = OccurrenceRef {
            kind: OccurrenceKind::CallSite,
            start_byte: 9_000,
            end_byte: 9_010,
        };
        let outcome = fixture
            .merge(&|generation| {
                app_request(
                    &fixture,
                    vec![
                        // No Occurrence at that span.
                        evidence(
                            &fixture,
                            &context(),
                            "src/app.ts",
                            Some(nowhere),
                            SemanticCapability::Overrides,
                            Some(RelationKind::Overrides),
                            Some(wired.clone()),
                            SemanticOutcome::Resolved {
                                target: handler.clone(),
                            },
                            generation,
                        ),
                        // No span at all.
                        evidence(
                            &fixture,
                            &context(),
                            "src/app.ts",
                            None,
                            SemanticCapability::Overrides,
                            Some(RelationKind::Overrides),
                            Some(wired.clone()),
                            SemanticOutcome::Resolved {
                                target: handler.clone(),
                            },
                            generation,
                        ),
                        // A target, but no relation kind: never inferred
                        // from the capability.
                        evidence(
                            &fixture,
                            &context(),
                            "src/app.ts",
                            Some(fixture.sites("src/app.ts", OccurrenceKind::CallSite)[2]),
                            SemanticCapability::TypeResolution,
                            None,
                            Some(wired.clone()),
                            SemanticOutcome::Resolved {
                                target: handler.clone(),
                            },
                            generation,
                        ),
                        // Candidates are not a proof.
                        evidence(
                            &fixture,
                            &context(),
                            "src/app.ts",
                            Some(fixture.sites("src/app.ts", OccurrenceKind::CallSite)[2]),
                            SemanticCapability::CallsCrossFile,
                            Some(RelationKind::Calls),
                            Some(wired.clone()),
                            SemanticOutcome::Candidates {
                                targets: vec![handler.clone()],
                            },
                            generation,
                        ),
                    ],
                )
            })
            .expect("merge");

        let reasons: Vec<RejectionReason> = outcome
            .rejected
            .iter()
            .map(|rejected| rejected.reason)
            .collect();
        assert_eq!(
            reasons,
            vec![
                RejectionReason::UnanchoredOccurrence,
                RejectionReason::NoOccurrence,
                RejectionReason::NoRelationKind,
                RejectionReason::NotResolved,
            ]
        );
        assert_eq!(
            fixture.counts(),
            before,
            "nothing was invented to hold them"
        );
    }

    #[test]
    fn a_dependency_target_becomes_an_external_entity_and_not_an_index() {
        let fixture = Fixture::create("external");
        let wired = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "wired"));
        let external = GraphEndpoint::External(ExternalEntity {
            package_identity: "lodash".to_owned(),
            module_path: Some("lodash/debounce".to_owned()),
            symbol_name: Some("debounce".to_owned()),
            qualified_name: Some("lodash/debounce.debounce".to_owned()),
            kind: "FUNCTION".to_owned(),
            resolved_version: Some("4.17.21".to_owned()),
            declaration_locator: Some("node_modules/lodash/debounce.js".to_owned()),
        });
        let resources_before: i64 = fixture
            .store()
            .connection()
            .query_row("SELECT COUNT(*) FROM resource", [], |row| row.get(0))
            .expect("count");

        fixture
            .merge(&|generation| {
                app_request(
                    &fixture,
                    vec![resolved_call(
                        &fixture,
                        &context(),
                        2,
                        wired.clone(),
                        external.clone(),
                        generation,
                    )],
                )
            })
            .expect("merge");

        let answer = fixture.outgoing(&wired);
        assert!(
            answer
                .confirmed
                .iter()
                .any(|result| result.target == external)
        );
        let resources_after: i64 = fixture
            .store()
            .connection()
            .query_row("SELECT COUNT(*) FROM resource", [], |row| row.get(0))
            .expect("count");
        assert_eq!(
            resources_before, resources_after,
            "resolving into a dependency does not index it"
        );
    }

    // -----------------------------------------------------------------
    // Replacement, withdrawal, downgrade
    // -----------------------------------------------------------------

    #[test]
    fn a_new_generation_replaces_the_previous_semantic_target() {
        let fixture = Fixture::create("replace-target");
        let wired = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "wired"));
        let handler = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "handler"));
        let maybe = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "maybe"));

        fixture
            .merge(&|generation| {
                app_request(
                    &fixture,
                    vec![resolved_call(
                        &fixture,
                        &context(),
                        2,
                        wired.clone(),
                        handler.clone(),
                        generation,
                    )],
                )
            })
            .expect("first merge");
        assert_eq!(fixture.callers_of(&handler), vec![wired.clone()]);

        // The backend changes its mind. The old target must not linger.
        let outcome = fixture
            .merge(&|generation| {
                app_request(
                    &fixture,
                    vec![resolved_call(
                        &fixture,
                        &context(),
                        2,
                        wired.clone(),
                        maybe.clone(),
                        generation,
                    )],
                )
            })
            .expect("second merge");
        assert_eq!(outcome.relations_removed, 1, "the old edge was collected");

        assert!(
            fixture.callers_of(&handler).is_empty(),
            "an old semantic target does not accumulate"
        );
        assert_eq!(fixture.callers_of(&maybe), vec![wired]);
        let rows: i64 = fixture
            .store()
            .connection()
            .query_row("SELECT COUNT(*) FROM semantic_evidence", [], |row| {
                row.get(0)
            })
            .expect("count");
        assert_eq!(rows, 1, "one site, one semantic proof");
    }

    #[test]
    fn withdrawing_a_binding_proof_restores_the_gap_it_displaced() {
        let fixture = Fixture::create("withdraw-binding");
        let wired = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "wired"));
        let handler = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "handler"));
        let before = fixture.counts();

        fixture
            .merge(&|generation| {
                app_request(
                    &fixture,
                    vec![resolved_call(
                        &fixture,
                        &context(),
                        2,
                        wired.clone(),
                        handler.clone(),
                        generation,
                    )],
                )
            })
            .expect("merge");

        // The backend's answer stops being current. What it proved on
        // its own stops being claimed, and the gap comes back saying
        // exactly what it said before.
        let outcome = fixture.withdraw(&context().context_key());
        assert_eq!(outcome.gaps_restored, 1);
        assert_eq!(outcome.relations_removed, 1);
        assert_eq!(fixture.counts(), before, "back to the structural baseline");

        let answer = fixture.outgoing(&wired);
        assert!(
            answer.confirmed.is_empty(),
            "a lost proof is not a preserved answer"
        );
        assert_eq!(answer.gaps.len(), 1);
        assert_eq!(
            answer.gaps[0].reason,
            UnresolvedReason::ReceiverTypeRequired
        );
        assert!(
            answer
                .coverage
                .limits()
                .has(CoverageLimit::UnresolvedEvidence)
        );
        assert!(
            !answer.coverage.limits().is_complete(),
            "an honest gap, never a false zero"
        );
    }

    #[test]
    fn structural_truth_survives_the_backend_leaving() {
        let fixture = Fixture::create("withdraw-corroboration");
        let run = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"));
        let save = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "save"));

        fixture
            .merge(&|generation| {
                app_request(
                    &fixture,
                    vec![resolved_call(
                        &fixture,
                        &context(),
                        0,
                        run.clone(),
                        save.clone(),
                        generation,
                    )],
                )
            })
            .expect("merge");

        let outcome = fixture.withdraw(&context().context_key());
        assert_eq!(outcome.gaps_restored, 0);
        assert_eq!(outcome.relations_removed, 0);

        let answer = fixture.outgoing(&run);
        assert!(
            answer
                .confirmed
                .iter()
                .any(|result| result.kind == RelationKind::Calls && result.target == save),
            "the parser proved this, and the parser has not gone anywhere"
        );
    }

    #[test]
    fn a_non_current_publication_cannot_touch_canonical_truth() {
        let fixture = Fixture::create("not-current");
        let wired = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "wired"));
        let handler = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "handler"));
        let before = fixture.counts();

        for state in [
            SemanticState::Building,
            SemanticState::Dirty,
            SemanticState::Unavailable,
            SemanticState::None,
        ] {
            let error = fixture
                .merge(&|generation| {
                    let mut request = app_request(
                        &fixture,
                        vec![resolved_call(
                            &fixture,
                            &context(),
                            2,
                            wired.clone(),
                            handler.clone(),
                            generation,
                        )],
                    );
                    request.status.state = state;
                    request
                })
                .expect_err("a non-current publication is refused");
            assert!(matches!(error, MergeError::NotCurrent { .. }), "{error:?}");
        }
        assert_eq!(fixture.counts(), before, "structural truth stands");
    }

    #[test]
    fn one_context_cannot_merge_into_anothers_contribution() {
        let fixture = Fixture::create("context-isolation");
        let wired = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "wired"));
        let handler = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "handler"));

        // Evidence produced for one AnalysisContext, offered as another's.
        let error = fixture
            .merge(&|generation| {
                let mut request = app_request(
                    &fixture,
                    vec![resolved_call(
                        &fixture,
                        &other_context(),
                        2,
                        wired.clone(),
                        handler.clone(),
                        generation,
                    )],
                );
                request.status = current_status(&context());
                request
            })
            .expect_err("mismatched context is refused");
        assert!(
            matches!(error, MergeError::ContextMismatch { .. }),
            "{error:?}"
        );

        // A different worktree is a different context, however
        // identical the project looks.
        assert_ne!(other_worktree().context_key(), context().context_key());
        let error = fixture
            .merge(&|generation| {
                let mut request = app_request(
                    &fixture,
                    vec![resolved_call(
                        &fixture,
                        &other_worktree(),
                        2,
                        wired.clone(),
                        handler.clone(),
                        generation,
                    )],
                );
                request.status = current_status(&context());
                request
            })
            .expect_err("another worktree is refused");
        assert!(
            matches!(error, MergeError::ContextMismatch { .. }),
            "{error:?}"
        );

        // And evidence owned by a different Resource likewise.
        let error = fixture
            .merge(&|generation| {
                let mut request = app_request(
                    &fixture,
                    vec![resolved_call(
                        &fixture,
                        &context(),
                        2,
                        wired.clone(),
                        handler.clone(),
                        generation,
                    )],
                );
                request.owner = fixture.resource("src/other.ts").id;
                request
            })
            .expect_err("mismatched owner is refused");
        assert!(
            matches!(error, MergeError::OwnerMismatch { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn merging_the_same_publication_twice_changes_nothing() {
        let fixture = Fixture::create("idempotent");
        let wired = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "wired"));
        let handler = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "handler"));
        let run = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"));
        let save = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "save"));

        let build = |generation| {
            app_request(
                &fixture,
                vec![
                    resolved_call(
                        &fixture,
                        &context(),
                        2,
                        wired.clone(),
                        handler.clone(),
                        generation,
                    ),
                    resolved_call(
                        &fixture,
                        &context(),
                        0,
                        run.clone(),
                        save.clone(),
                        generation,
                    ),
                ],
            )
        };

        let first = fixture.merge(&build).expect("first merge");
        let counts = fixture.counts();
        let answer = format!("{:?}", fixture.outgoing(&wired).confirmed);

        let second = fixture.merge(&build).expect("second merge");
        assert_eq!(fixture.counts(), counts, "no row accumulated");
        assert_eq!(format!("{:?}", fixture.outgoing(&wired).confirmed), answer);
        assert_eq!(second.relations_created, 0);
        assert_eq!(second.gaps_resolved, 0, "the gap was already resolved");
        assert_eq!(second.gaps_restored, 0);
        assert_eq!(second.relations_removed, 0);
        assert_eq!(first.conflicts, second.conflicts);
    }

    #[test]
    fn replacing_one_resources_contribution_leaves_another_resources_alone() {
        let fixture = Fixture::create("resource-ownership");
        let wired = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "wired"));
        let handler = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "handler"));
        let keep = GraphEndpoint::Symbol(fixture.symbol("src/other.ts", "keep"));
        let persist = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "persist"));

        fixture
            .merge(&|generation| {
                app_request(
                    &fixture,
                    vec![resolved_call(
                        &fixture,
                        &context(),
                        2,
                        wired.clone(),
                        handler.clone(),
                        generation,
                    )],
                )
            })
            .expect("merge");

        // Re-merging app.ts with nothing at all withdraws only app.ts's
        // contribution.
        fixture
            .merge(&|_| app_request(&fixture, Vec::new()))
            .expect("empty merge");

        assert!(fixture.callers_of(&handler).is_empty());
        assert_eq!(
            fixture.callers_of(&persist),
            vec![keep],
            "another Resource's structural evidence is untouched"
        );
    }

    #[test]
    fn a_relation_is_collected_only_when_nothing_proves_it_any_more() {
        let fixture = Fixture::create("gc");
        let run = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"));
        let wired = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "wired"));
        let save = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "save"));

        // A semantic proof of an edge the parser already proved, plus a
        // semantic-only one.
        fixture
            .merge(&|generation| {
                app_request(
                    &fixture,
                    vec![
                        resolved_call(
                            &fixture,
                            &context(),
                            0,
                            run.clone(),
                            save.clone(),
                            generation,
                        ),
                        resolved_call(
                            &fixture,
                            &context(),
                            2,
                            wired.clone(),
                            save.clone(),
                            generation,
                        ),
                    ],
                )
            })
            .expect("merge");

        let outcome = fixture.withdraw(&context().context_key());
        assert_eq!(
            outcome.relations_removed, 1,
            "only the edge whose last proof went away"
        );
        assert!(
            fixture.callers_of(&save).contains(&run),
            "the structurally proven edge survives"
        );
        assert!(!fixture.callers_of(&save).contains(&wired));
    }

    // -----------------------------------------------------------------
    // Query integration and coverage
    // -----------------------------------------------------------------

    #[test]
    fn every_existing_query_surface_sees_the_enriched_graph() {
        let fixture = Fixture::create("queries");
        let wired = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "wired"));
        let handler = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "handler"));
        fixture
            .merge(&|generation| {
                app_request(
                    &fixture,
                    vec![resolved_call(
                        &fixture,
                        &context(),
                        2,
                        wired.clone(),
                        handler.clone(),
                        generation,
                    )],
                )
            })
            .expect("merge");

        // Direct query: no semantic-specific API anywhere.
        assert_eq!(fixture.callers_of(&handler), vec![wired.clone()]);

        // Impact traversal walks the same canonical rows.
        let traversal = ImpactTraversal::open(&fixture.db_path()).expect("index.db");
        let result = traversal
            .run(
                ImpactIntent::PublicSignatureChange,
                &handler,
                &Budget::default(),
            )
            .expect("impact");
        let reached: Vec<&GraphEndpoint> = result.nodes.iter().map(|node| &node.endpoint).collect();
        assert!(
            reached.contains(&&wired),
            "impact reaches a semantically resolved caller with no special path"
        );

        // Related-test projection walks the same confirmed paths.
        let projection = RelatedTests::open(&fixture.db_path())
            .expect("index.db")
            .for_target(
                &handler,
                ImpactIntent::PublicSignatureChange,
                &Budget::default(),
            )
            .expect("projection");
        assert!(
            projection
                .candidates
                .iter()
                .any(|candidate| candidate.path_rel == "src/app.test.ts"),
            "a test reaches the change through the semantically resolved edge"
        );

        // Prepared source reads the current exact span of the
        // semantically resolved evidence.
        let prepared = InspectPreparer::open(&fixture.db_path(), &fixture.root)
            .expect("index.db")
            .prepare(&handler, Direction::Incoming, &[RelationKind::Calls])
            .expect("prepare");
        assert!(prepared.source_complete());
        assert!(
            prepared
                .ranges
                .iter()
                .any(|range| range.source.contains("handler()")),
            "the prepared source is the real current site, not a remembered one"
        );
    }

    #[test]
    fn coverage_states_what_the_semantic_tier_cannot_currently_say() {
        let fixture = Fixture::create("coverage");
        let run = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"));
        let persist = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "persist"));

        // Nothing semantic has happened: nothing semantic is claimed.
        let answer = fixture.outgoing(&run);
        assert!(answer.coverage.semantic.is_empty());
        assert!(
            !answer
                .coverage
                .limits()
                .has(CoverageLimit::SemanticConflict)
        );
        assert!(
            !answer
                .coverage
                .limits()
                .has(CoverageLimit::SemanticNotCurrent)
        );

        fixture
            .merge(&|generation| {
                app_request(
                    &fixture,
                    vec![resolved_call(
                        &fixture,
                        &context(),
                        0,
                        run.clone(),
                        persist.clone(),
                        generation,
                    )],
                )
            })
            .expect("merge");

        // A contributing context with no published component is not a
        // current one, and the report says so rather than staying silent.
        let answer = fixture.outgoing(&run);
        assert_eq!(answer.coverage.semantic.contexts, 1);
        assert_eq!(answer.coverage.semantic.conflicts, 1);
        assert!(answer.coverage.semantic.not_current);
        let limits = answer.coverage.limits();
        assert!(limits.has(CoverageLimit::SemanticConflict));
        assert!(limits.has(CoverageLimit::SemanticNotCurrent));
        assert!(!limits.is_complete());

        // Still no score anywhere: a limit is present or it is not.
        assert!(
            format!("{limits:?}").find("confidence").is_none(),
            "coverage is a closed vocabulary, not a number"
        );
    }

    #[test]
    fn a_refused_merge_leaves_no_half_applied_state() {
        let fixture = Fixture::create("atomic");
        let wired = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "wired"));
        let handler = GraphEndpoint::Symbol(fixture.symbol("src/store.ts", "handler"));
        let before = fixture.counts();

        // The first item would resolve a gap; the second names another
        // context and refuses the whole merge.
        let error = fixture
            .merge(&|generation| {
                let mut request = app_request(
                    &fixture,
                    vec![
                        resolved_call(
                            &fixture,
                            &context(),
                            2,
                            wired.clone(),
                            handler.clone(),
                            generation,
                        ),
                        resolved_call(
                            &fixture,
                            &other_context(),
                            0,
                            wired.clone(),
                            handler.clone(),
                            generation,
                        ),
                    ],
                );
                request.status = current_status(&context());
                request
            })
            .expect_err("refused");
        assert!(matches!(error, MergeError::ContextMismatch { .. }));

        assert_eq!(
            fixture.counts(),
            before,
            "a reader never sees the gap and its resolution at once"
        );
        let answer = fixture.outgoing(&wired);
        assert!(answer.confirmed.is_empty());
        assert_eq!(answer.gaps.len(), 1);
    }
}
