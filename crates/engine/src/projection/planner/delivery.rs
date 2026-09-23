//! Hard delivery budget + continuation (#20 task 7).
//!
//! Cuts a [`PreparedProjection`] into bounded pages without cutting
//! correctness: the required integrity bundle is one atomic group on the
//! first page (or an explicit error), then optional units follow in a
//! fixed priority order until a cap would be exceeded. Optional source is
//! read only after the budget selected it.
//!
//! A continuation is typed Core state, never a transport string and never
//! persisted: the index incarnation, Workspace revision, stable generation,
//! request, projection and budget it was cut for, plus the key of the next
//! unit. Anything else current is a hard mismatch.
//!
//! What this is not: delivery accounting, a client/session ledger, or
//! same-revision reuse (task 8), and not a transport (task 10+). Bytes
//! are the canonical payload bytes of the delivered units; the page's own
//! basis fields are framing and are not counted.

use std::{collections::BTreeSet, error::Error, fmt, num::NonZeroUsize};

use brainprint_core::{IndexIncarnationId, ProjectId, WorkspaceId};

use super::{
    PlannedSourceRange, PlannerError, PreparedProjection, ProjectionGap, ProjectionPlanner,
    Relevance, ReuseReference, SourceRequirement, merge,
};
use crate::{
    generation::GenerationError,
    graph::GraphEndpoint,
    inspect::SourceVerification,
    knowledge::{GenerationReferenceState, ResolutionReason},
    prepare::PreparedRange,
    projection::{
        EvidenceItem, ProjectionIntent, ProjectionRequest, ProjectionRequestError,
        canonical::{self, Canonical},
    },
    query::Currentness,
};

const REQUEST_DOMAIN: &str = "brainprint.projection-request.v1";
const PROJECTION_DOMAIN: &str = "brainprint.prepared-projection.v1";
const UNIT_DOMAIN: &str = "brainprint.delivery-unit.v1";

// ---------------------------------------------------------------- budget

/// A hard cap on one page. Explicit only: at least one cap, no zero cap,
/// and no `Default` -- a budget comes from the caller, a profile, or a
/// benchmark, never from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryBudget {
    max_items: Option<NonZeroUsize>,
    max_bytes: Option<NonZeroUsize>,
    max_tokens: Option<NonZeroUsize>,
}

impl DeliveryBudget {
    pub fn new(
        max_items: Option<usize>,
        max_bytes: Option<usize>,
        max_tokens: Option<usize>,
    ) -> Result<Self, DeliveryError> {
        let cap = |dimension, value: Option<usize>| {
            value
                .map(|value| {
                    NonZeroUsize::new(value).ok_or(DeliveryError::ZeroBudgetCap(dimension))
                })
                .transpose()
        };
        let budget = Self {
            max_items: cap(DeliveryDimension::Items, max_items)?,
            max_bytes: cap(DeliveryDimension::Bytes, max_bytes)?,
            max_tokens: cap(DeliveryDimension::Tokens, max_tokens)?,
        };
        if budget.max_items.is_none() && budget.max_bytes.is_none() && budget.max_tokens.is_none() {
            return Err(DeliveryError::NoBudgetCap);
        }
        Ok(budget)
    }

    #[must_use]
    pub const fn max_items(&self) -> Option<NonZeroUsize> {
        self.max_items
    }

    #[must_use]
    pub const fn max_bytes(&self) -> Option<NonZeroUsize> {
        self.max_bytes
    }

    #[must_use]
    pub const fn max_tokens(&self) -> Option<NonZeroUsize> {
        self.max_tokens
    }

    /// The caps `used + cost` would exceed.
    fn over(&self, used: Cost, cost: Cost) -> BTreeSet<DeliveryDimension> {
        let exceeds = |cap: Option<NonZeroUsize>, total: Option<usize>| matches!((cap, total), (Some(cap), Some(total)) if total > cap.get());
        let add = |a: Option<usize>, b: Option<usize>| Some(a? + b?);
        [
            (
                DeliveryDimension::Items,
                exceeds(self.max_items, Some(used.items + cost.items)),
            ),
            (
                DeliveryDimension::Bytes,
                exceeds(self.max_bytes, Some(used.bytes + cost.bytes)),
            ),
            (
                DeliveryDimension::Tokens,
                exceeds(self.max_tokens, add(used.tokens, cost.tokens)),
            ),
        ]
        .into_iter()
        .filter_map(|(dimension, over)| over.then_some(dimension))
        .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeliveryDimension {
    Items,
    Bytes,
    Tokens,
}

/// One unit an [`ExactTokenCounter`] is asked about.
#[derive(Debug, Clone, Copy)]
pub enum DeliveryUnit<'a> {
    Evidence(&'a EvidenceItem),
    Gap(&'a ProjectionGap),
}

/// An exact, cheap token counter the caller supplies. `None` means the
/// exact count is not known for that unit; nothing here estimates.
pub trait ExactTokenCounter {
    fn exact_tokens(&self, unit: DeliveryUnit<'_>) -> Option<usize>;

    /// The exact token cost of the `CurrentSource` an optional range will
    /// become, when the caller already knows it without its body (e.g.
    /// precomputed metadata). Under a token cap an optional range is
    /// selected only with this; it is never read to find out.
    fn exact_planned_source_tokens(&self, _range: &PlannedSourceRange) -> Option<usize> {
        None
    }

    /// The exact token cost of a [`ReuseReference`] (task 8). Under a
    /// token cap a reference is used only with this; otherwise the full
    /// payload is delivered.
    fn exact_reuse_tokens(&self, _reference: &ReuseReference) -> Option<usize> {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenUsage {
    Known(usize),
    Unknown,
}

/// Items / canonical bytes / exact tokens (`None` = not known).
#[derive(Debug, Clone, Copy)]
struct Cost {
    items: usize,
    bytes: usize,
    tokens: Option<usize>,
}

impl Cost {
    const ZERO: Self = Self {
        items: 0,
        bytes: 0,
        tokens: Some(0),
    };

    fn plus(self, other: Self) -> Self {
        Self {
            items: self.items + other.items,
            bytes: self.bytes + other.bytes,
            tokens: self.tokens.zip(other.tokens).map(|(a, b)| a + b),
        }
    }
}

// ---------------------------------------------------------- continuation

/// Where a page chain resumes: the next unit's priority tier, impact
/// depth, and the SHA-256 of its canonical encoding. A key, not an offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryKey {
    pub(crate) tier: u8,
    pub(crate) depth: usize,
    pub(crate) identity: [u8; 32],
}

/// Compact typed state for the next page. No payload, no source, no list
/// of remaining ids; never persisted or transport-encoded here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryContinuation {
    pub(crate) workspace: WorkspaceId,
    pub(crate) index_incarnation: IndexIncarnationId,
    pub(crate) workspace_revision: String,
    pub(crate) generation_no: i64,
    pub(crate) generation_basis_revision: String,
    pub(crate) request_fingerprint: [u8; 32],
    pub(crate) projection_fingerprint: [u8; 32],
    pub(crate) budget: DeliveryBudget,
    pub(crate) next: DeliveryKey,
}

/// Why a page with more available carries no continuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationUnavailable {
    /// No stable generation (or no revision clock) to bind one to.
    NoStableGeneration,
    /// The next unit exceeds the whole budget on its own; this chain can
    /// never deliver it. A fresh chain needs a larger budget.
    UnitExceedsBudget,
    /// A token cap, and the next optional source range has no exact
    /// planned token cost: it is not read to find out. A fresh chain needs
    /// a preflight-capable counter or a budget without a token cap.
    OptionalSourceTokenCostUnknown,
}

/// Which binding of a continuation no longer holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationMismatch {
    Workspace,
    IndexIncarnation,
    WorkspaceRevision,
    StableGeneration,
    Request,
    /// Re-planned truth differs (Policy, Decision, Working State, ...).
    Projection,
    Budget,
}

// ------------------------------------------------------------------ page

/// One bounded page. Engine-internal; not a transport shape and not
/// delivery accounting.
#[derive(Debug, Clone, PartialEq)]
pub struct DeliveryPage {
    pub workspace: WorkspaceId,
    pub project: ProjectId,
    pub intent: ProjectionIntent,
    pub target: Option<GraphEndpoint>,
    /// Required bundle (first page only), then optional units in
    /// delivery order.
    pub evidence: Vec<EvidenceItem>,
    /// One per `evidence` item: `Some` when that item is delivered as a
    /// reference to an acknowledged identical payload instead of in full
    /// (task 8). The item itself stays the full canonical truth.
    pub references: Vec<Option<ReuseReference>>,
    /// Every planner gap, on the first page only.
    pub gaps: Vec<ProjectionGap>,
    pub used_items: usize,
    /// Canonical payload bytes of the units above.
    pub used_bytes: usize,
    pub used_tokens: TokenUsage,
    /// True only when optional units remain undelivered.
    pub more_available: bool,
    /// The caps the next unit did not fit; empty when nothing remains or
    /// when no cap was proven exceeded (unknown optional-source tokens).
    pub limiting: BTreeSet<DeliveryDimension>,
    /// Exact count of optional units not yet delivered in this chain.
    pub omitted_units: usize,
    pub continuation: Option<DeliveryContinuation>,
    pub continuation_unavailable: Option<ContinuationUnavailable>,
}

// ----------------------------------------------------------------- error

#[derive(Debug)]
pub enum DeliveryError {
    NoBudgetCap,
    ZeroBudgetCap(DeliveryDimension),
    /// `max_tokens` without an exact counter: never estimated.
    TokenCounterRequired,
    /// The counter has no exact count for a unit under a token cap.
    TokenCostUnknown,
    /// The required integrity bundle alone does not fit.
    BudgetTooSmallForRequiredEvidence {
        required_items: usize,
        required_bytes: usize,
        /// Only when an exact counter counted every required unit.
        required_tokens: Option<usize>,
        violated: BTreeSet<DeliveryDimension>,
    },
    /// The projection was not planned for this request/Workspace.
    ProjectionNotForRequest,
    ContinuationMismatch(ContinuationMismatch),
    /// The bindings hold but the cursor key is not in this sequence.
    InvalidContinuationCursor,
    /// A range selected on its exact planned token cost materialized at
    /// another exact cost: the planned figure was not exact.
    TokenPreflightMismatch {
        planned: usize,
        materialized: Option<usize>,
    },
    /// A selected optional range failed verification and its
    /// `SourceUnavailable` does not fit even an empty page.
    BudgetTooSmallForSelectedCorrectiveEvidence {
        unit_bytes: usize,
        unit_tokens: Option<usize>,
        violated: BTreeSet<DeliveryDimension>,
    },
    /// FreshContext means earlier pages may be gone; a continuation
    /// assumes they are retained.
    FreshContextWithContinuation,
    Planner(PlannerError),
}

impl fmt::Display for DeliveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoBudgetCap => formatter.write_str("a delivery budget needs at least one cap"),
            Self::ZeroBudgetCap(dimension) => write!(formatter, "{dimension:?} cap is zero"),
            Self::TokenCounterRequired => {
                formatter.write_str("a token cap needs an exact token counter")
            }
            Self::TokenCostUnknown => {
                formatter.write_str("exact token cost unknown for a unit under a token cap")
            }
            Self::BudgetTooSmallForRequiredEvidence {
                required_items,
                required_bytes,
                required_tokens,
                violated,
            } => write!(
                formatter,
                "budget too small for required evidence ({required_items} items, \
                 {required_bytes} bytes, tokens {required_tokens:?}; over {violated:?})"
            ),
            Self::ProjectionNotForRequest => {
                formatter.write_str("the projection was not planned for this request")
            }
            Self::ContinuationMismatch(mismatch) => {
                write!(
                    formatter,
                    "continuation does not match current {mismatch:?}"
                )
            }
            Self::InvalidContinuationCursor => {
                formatter.write_str("continuation cursor is not in this delivery sequence")
            }
            Self::TokenPreflightMismatch {
                planned,
                materialized,
            } => write!(
                formatter,
                "planned source tokens {planned} but materialized {materialized:?}"
            ),
            Self::BudgetTooSmallForSelectedCorrectiveEvidence {
                unit_bytes,
                unit_tokens,
                violated,
            } => write!(
                formatter,
                "selected source is unavailable and its corrective unit ({unit_bytes} bytes, \
                 tokens {unit_tokens:?}) exceeds the whole budget ({violated:?})"
            ),
            Self::FreshContextWithContinuation => {
                formatter.write_str("a fresh context cannot continue an earlier page chain")
            }
            Self::Planner(source) => write!(formatter, "{source}"),
        }
    }
}

impl Error for DeliveryError {}

macro_rules! via_planner {
    ($($source:ty),*) => {
        $(impl From<$source> for DeliveryError {
            fn from(source: $source) -> Self {
                Self::Planner(source.into())
            }
        })*
    };
}

via_planner!(PlannerError, ProjectionRequestError, GenerationError);

// -------------------------------------------------------------- sequence

enum Slot<'a> {
    Evidence(&'a EvidenceItem),
    Source(PlannedSourceRange),
}

struct Entry<'a> {
    key: DeliveryKey,
    slot: Slot<'a>,
}

/// The required bundle when `intent` is being delivered (§4, §8).
/// Every Coverage is required, so a relation/test family never reaches
/// the Agent without the coverage that bounds it.
fn required(item: &EvidenceItem, intent: ProjectionIntent) -> bool {
    let resume = intent == ProjectionIntent::ResumeHandoff;
    match item {
        EvidenceItem::Resource(_)
        | EvidenceItem::Symbol(_)
        | EvidenceItem::TargetSelection(_)
        | EvidenceItem::CurrentSource(_)
        | EvidenceItem::Coverage(_)
        | EvidenceItem::SourceUnavailable { .. }
        | EvidenceItem::KnowledgeConflict(_)
        | EvidenceItem::Directive(_) => true,
        EvidenceItem::IndexCurrentness { currentness, .. } => !currentness.is_current(),
        EvidenceItem::Policy(entry) => entry.reason == ResolutionReason::ProtectedConstraint,
        EvidenceItem::WorkItem(_) | EvidenceItem::WorkingState(_) | EvidenceItem::Handoff(_) => {
            resume
        }
        EvidenceItem::GenerationReference { reference, .. } => {
            resume && reference.state != GenerationReferenceState::PresentMatching
        }
        _ => false,
    }
}

/// Optional priority tier (§6): 1 target remainder, 2 direct, 3 rules and
/// requested knowledge, 4 related tests, 5 coverage support, 6 transitive
/// (then by depth), 7 Working State, 8 optional source.
const fn tier(relevance: Relevance) -> u8 {
    match relevance {
        Relevance::Target => 1,
        Relevance::DirectRelation => 2,
        Relevance::ApplicableRule | Relevance::RequestedKnowledge => 3,
        Relevance::RelatedTest => 4,
        Relevance::CoverageSupport | Relevance::Corrective => 5,
        Relevance::TransitiveImpact => 6,
        Relevance::WorkingState => 7,
    }
}

const SOURCE_TIER: u8 = 8;

fn key<T: Canonical + ?Sized>(tier: u8, depth: usize, unit: &T) -> DeliveryKey {
    DeliveryKey {
        tier,
        depth,
        identity: canonical::digest(UNIT_DOMAIN, unit),
    }
}

/// Required evidence (projection order) and the optional sequence in
/// delivery order. Optional source candidates are merged here, before
/// any read; a candidate inside an already delivered required range is
/// dropped as a duplicate.
fn sequence(projection: &PreparedProjection) -> (Vec<&EvidenceItem>, Vec<Entry<'_>>) {
    let mut required_items = Vec::new();
    let mut optional = Vec::new();
    for (item, hint) in projection.evidence.iter().zip(&projection.delivery) {
        if required(item, projection.intent) {
            required_items.push(item);
        } else {
            let depth = hint.impact_depth.unwrap_or(0);
            optional.push(Entry {
                key: key(tier(hint.relevance), depth, item),
                slot: Slot::Evidence(item),
            });
        }
    }
    // Stable: same tier and depth keep the planner's canonical order.
    optional.sort_by_key(|entry| (entry.key.tier, entry.key.depth));

    let (required_ranges, candidates): (Vec<_>, Vec<_>) = projection
        .source_plan
        .iter()
        .cloned()
        .partition(|range| range.requirement == SourceRequirement::Required);
    let within_required = |range: &PlannedSourceRange| {
        required_ranges.iter().any(|outer| {
            outer.resource == range.resource
                && outer.resource_revision == range.resource_revision
                && outer.span.start_byte <= range.span.start_byte
                && range.span.end_byte <= outer.span.end_byte
        })
    };
    for range in merge(&candidates) {
        if !within_required(&range) {
            optional.push(Entry {
                key: key(SOURCE_TIER, 0, &range),
                slot: Slot::Source(range),
            });
        }
    }
    (required_items, optional)
}

// ------------------------------------------------------------------ page

/// For a materialized item, the reference to an acknowledged identical
/// payload in the caller's retained context, if there is one.
pub(super) type ReuseLookup<'a> = &'a dyn Fn(&EvidenceItem) -> Option<ReuseReference>;

struct Builder<'a> {
    budget: &'a DeliveryBudget,
    counter: Option<&'a dyn ExactTokenCounter>,
    reuse: Option<ReuseLookup<'a>>,
    evidence: Vec<EvidenceItem>,
    references: Vec<Option<ReuseReference>>,
    used: Cost,
}

impl Builder<'_> {
    fn cost(&self, unit: DeliveryUnit<'_>) -> Result<Cost, DeliveryError> {
        let bytes = match unit {
            DeliveryUnit::Evidence(item) => canonical::size(item),
            DeliveryUnit::Gap(gap) => canonical::size(gap),
        };
        let tokens = self.counter.and_then(|counter| counter.exact_tokens(unit));
        if self.budget.max_tokens.is_some() && tokens.is_none() {
            return Err(DeliveryError::TokenCostUnknown);
        }
        Ok(Cost {
            items: 1,
            bytes,
            tokens,
        })
    }

    /// How `item` is delivered and what that costs: a reference when the
    /// retained context holds the identical payload, the reference is
    /// strictly smaller, and -- under a token cap -- its exact token cost
    /// is known; otherwise the full payload.
    fn represent(
        &self,
        item: &EvidenceItem,
    ) -> Result<(Cost, Option<ReuseReference>), DeliveryError> {
        if let Some(reference) = self.reuse.and_then(|lookup| lookup(item)) {
            let bytes = canonical::size(&reference);
            let tokens = self
                .counter
                .and_then(|counter| counter.exact_reuse_tokens(&reference));
            if bytes < canonical::size(item)
                && (self.budget.max_tokens.is_none() || tokens.is_some())
            {
                let cost = Cost {
                    items: 1,
                    bytes,
                    tokens,
                };
                return Ok((cost, Some(reference)));
            }
        }
        Ok((self.cost(DeliveryUnit::Evidence(item))?, None))
    }

    /// Add `item` if it fits; otherwise the caps it would exceed, and
    /// whether it exceeds the whole budget on its own.
    fn offer(&mut self, item: EvidenceItem) -> Result<Option<Stop>, DeliveryError> {
        let (cost, reference) = self.represent(&item)?;
        let over = self.budget.over(self.used, cost);
        if over.is_empty() {
            self.used = self.used.plus(cost);
            self.evidence.push(item);
            self.references.push(reference);
            return Ok(None);
        }
        Ok(Some(Stop {
            never_fits: !self.budget.over(Cost::ZERO, cost).is_empty(),
            limiting: over,
            token_cost_unknown: false,
        }))
    }
}

/// Why the page ended before the sequence did.
struct Stop {
    limiting: BTreeSet<DeliveryDimension>,
    never_fits: bool,
    /// A token cap, and the next optional range has no exact planned cost.
    token_cost_unknown: bool,
}

/// What the generation store says now.
struct Basis {
    incarnation: IndexIncarnationId,
    revision: Option<String>,
    stable: Option<(i64, String)>,
}

impl ProjectionPlanner {
    /// Deliver one bounded page of `projection`, which must be the
    /// projection this planner prepared for `request`. `continuation` is
    /// `None` for the first page. A token cap needs `tokens`.
    pub fn deliver(
        &self,
        request: &ProjectionRequest,
        projection: &PreparedProjection,
        budget: &DeliveryBudget,
        continuation: Option<&DeliveryContinuation>,
        tokens: Option<&dyn ExactTokenCounter>,
    ) -> Result<DeliveryPage, DeliveryError> {
        self.deliver_with(request, projection, budget, continuation, tokens, None)
            .map(|(page, _)| page)
    }

    /// [`Self::deliver`] with an optional retained-context lookup (task 8),
    /// also returning the merged optional source candidates it cut from.
    pub(super) fn deliver_with(
        &self,
        request: &ProjectionRequest,
        projection: &PreparedProjection,
        budget: &DeliveryBudget,
        continuation: Option<&DeliveryContinuation>,
        tokens: Option<&dyn ExactTokenCounter>,
        reuse: Option<ReuseLookup<'_>>,
    ) -> Result<(DeliveryPage, Vec<PlannedSourceRange>), DeliveryError> {
        request.validate()?;
        if request.workspace != self.workspace_id {
            return Err(PlannerError::WorkspaceMismatch {
                bound: self.workspace_id,
                requested: request.workspace,
            }
            .into());
        }
        if projection.workspace != self.workspace_id
            || projection.project != self.project_id
            || projection.intent != request.intent
            || projection.delivery.len() != projection.evidence.len()
        {
            return Err(DeliveryError::ProjectionNotForRequest);
        }
        if budget.max_tokens.is_some() && tokens.is_none() {
            return Err(DeliveryError::TokenCounterRequired);
        }

        let basis = self.basis()?;
        let request_fingerprint = canonical::digest(REQUEST_DOMAIN, request);
        let projection_fingerprint = canonical::digest(PROJECTION_DOMAIN, projection);
        let (required_items, optional) = sequence(projection);
        let sources = optional
            .iter()
            .filter(|entry| matches!(entry.slot, Slot::Source(_)))
            .count();
        self.count(|stats| stats.optional_source_candidates += sources as u64);

        let mut page = Builder {
            budget,
            counter: tokens,
            reuse,
            evidence: Vec::new(),
            references: Vec::new(),
            used: Cost {
                tokens: tokens.map(|_| 0),
                ..Cost::ZERO
            },
        };
        let (start, gaps) = match continuation {
            None => {
                let mut total = page.used;
                for gap in &projection.gaps {
                    total = total.plus(page.cost(DeliveryUnit::Gap(gap))?);
                }
                let mut references = Vec::with_capacity(required_items.len());
                for item in &required_items {
                    let (cost, reference) = page.represent(item)?;
                    total = total.plus(cost);
                    references.push(reference);
                }
                let violated = budget.over(Cost::ZERO, total);
                if !violated.is_empty() {
                    return Err(DeliveryError::BudgetTooSmallForRequiredEvidence {
                        required_items: total.items,
                        required_bytes: total.bytes,
                        required_tokens: total.tokens,
                        violated,
                    });
                }
                page.used = total;
                page.evidence.extend(required_items.into_iter().cloned());
                page.references = references;
                (0, projection.gaps.clone())
            }
            Some(continuation) => {
                check(
                    continuation,
                    self.workspace_id,
                    &basis,
                    request_fingerprint,
                    projection_fingerprint,
                    budget,
                )?;
                let start = optional
                    .iter()
                    .position(|entry| entry.key == continuation.next)
                    .ok_or(DeliveryError::InvalidContinuationCursor)?;
                (start, Vec::new())
            }
        };

        let (end, stop) = self.fill(&mut page, &optional, start)?;
        let more_available = end < optional.len();
        let (continuation, continuation_unavailable) = match (&stop, &basis) {
            _ if !more_available => (None, None),
            (Some(stop), _) if stop.never_fits => {
                (None, Some(ContinuationUnavailable::UnitExceedsBudget))
            }
            (Some(stop), _) if stop.token_cost_unknown => (
                None,
                Some(ContinuationUnavailable::OptionalSourceTokenCostUnknown),
            ),
            (
                _,
                Basis {
                    incarnation,
                    revision: Some(revision),
                    stable: Some((generation_no, generation_basis_revision)),
                },
            ) => (
                Some(DeliveryContinuation {
                    workspace: self.workspace_id,
                    index_incarnation: *incarnation,
                    workspace_revision: revision.clone(),
                    generation_no: *generation_no,
                    generation_basis_revision: generation_basis_revision.clone(),
                    request_fingerprint,
                    projection_fingerprint,
                    budget: *budget,
                    next: optional[end].key.clone(),
                }),
                None,
            ),
            _ => (None, Some(ContinuationUnavailable::NoStableGeneration)),
        };

        let sources = optional
            .iter()
            .filter_map(|entry| match &entry.slot {
                Slot::Source(range) => Some(range.clone()),
                Slot::Evidence(_) => None,
            })
            .collect();
        let page = DeliveryPage {
            workspace: projection.workspace,
            project: projection.project,
            intent: projection.intent,
            target: projection.target.clone(),
            evidence: page.evidence,
            references: page.references,
            gaps,
            used_items: page.used.items,
            used_bytes: page.used.bytes,
            used_tokens: page
                .used
                .tokens
                .map_or(TokenUsage::Unknown, TokenUsage::Known),
            more_available,
            limiting: stop.map(|stop| stop.limiting).unwrap_or_default(),
            omitted_units: optional.len() - end,
            continuation,
            continuation_unavailable,
        };
        Ok((page, sources))
    }

    fn basis(&self) -> Result<Basis, DeliveryError> {
        Ok(Basis {
            incarnation: self.generations.index_incarnation_id()?,
            revision: self.generations.current_workspace_revision()?,
            stable: self
                .generations
                .current_stable()?
                .map(|record| (record.generation_no, record.basis_workspace_revision)),
        })
    }

    /// Add optional units from `start` until one does not fit. Returns the
    /// index of the first undelivered unit and why it stopped.
    fn fill(
        &self,
        page: &mut Builder<'_>,
        optional: &[Entry<'_>],
        start: usize,
    ) -> Result<(usize, Option<Stop>), DeliveryError> {
        let mut index = start;
        while index < optional.len() {
            match &optional[index].slot {
                Slot::Evidence(item) => {
                    if let Some(stop) = page.offer((*item).clone())? {
                        return Ok((index, Some(stop)));
                    }
                    index += 1;
                }
                Slot::Source(_) => {
                    // Budget before read: select by a lower bound of each
                    // unit's canonical bytes -- everything but the path and
                    // hashes the read will add -- and, under a token cap,
                    // only by an exact planned token cost.
                    let token_cap = page.budget.max_tokens.is_some();
                    let mut selected = Vec::new();
                    let mut planned = Vec::new();
                    let mut bound = page.used;
                    let mut preflight = None;
                    for entry in &optional[index..] {
                        let Slot::Source(range) = &entry.slot else {
                            break;
                        };
                        let tokens = if token_cap {
                            let exact = page
                                .counter
                                .and_then(|counter| counter.exact_planned_source_tokens(range));
                            if exact.is_none() {
                                preflight = Some(Stop {
                                    limiting: BTreeSet::new(),
                                    never_fits: false,
                                    token_cost_unknown: true,
                                });
                                break;
                            }
                            exact
                        } else {
                            None
                        };
                        let cost = Cost {
                            items: 1,
                            bytes: source_floor(range),
                            tokens,
                        };
                        let over = page.budget.over(bound, cost);
                        if !over.is_empty() {
                            preflight = Some(Stop {
                                never_fits: !page.budget.over(Cost::ZERO, cost).is_empty(),
                                limiting: over,
                                token_cost_unknown: false,
                            });
                            break;
                        }
                        bound = bound.plus(cost);
                        selected.push(range.clone());
                        planned.push(tokens);
                    }
                    self.count(|stats| stats.optional_source_selected += selected.len() as u64);
                    if selected.is_empty() {
                        return Ok((index, preflight));
                    }
                    // Sorted by (Resource, revision, span): one verified read
                    // per Resource revision, and no further read once one
                    // does not fit.
                    let mut planned = planned.into_iter();
                    for group in selected.chunk_by(|a, b| {
                        a.resource == b.resource && a.resource_revision == b.resource_revision
                    }) {
                        for item in self.read(group)? {
                            let planned = planned.next().flatten();
                            if let (Some(planned), EvidenceItem::CurrentSource(_)) =
                                (planned, &item)
                            {
                                let materialized = page.counter.and_then(|counter| {
                                    counter.exact_tokens(DeliveryUnit::Evidence(&item))
                                });
                                if materialized != Some(planned) {
                                    return Err(DeliveryError::TokenPreflightMismatch {
                                        planned,
                                        materialized,
                                    });
                                }
                            }
                            let corrective = matches!(item, EvidenceItem::SourceUnavailable { .. });
                            let cost = page.cost(DeliveryUnit::Evidence(&item))?;
                            if let Some(stop) = page.offer(item)? {
                                if corrective && stop.never_fits {
                                    return Err(
                                        DeliveryError::BudgetTooSmallForSelectedCorrectiveEvidence {
                                            unit_bytes: cost.bytes,
                                            unit_tokens: cost.tokens,
                                            violated: page.budget.over(Cost::ZERO, cost),
                                        },
                                    );
                                }
                                return Ok((index, Some(stop)));
                            }
                            index += 1;
                        }
                    }
                    if let Some(stop) = preflight {
                        return Ok((index, Some(stop)));
                    }
                }
            }
        }
        Ok((index, None))
    }
}

/// A lower bound of `range`'s delivered canonical bytes, known before
/// reading: the unit with an empty path, empty hashes, and a source of
/// exactly the span's length.
fn source_floor(range: &PlannedSourceRange) -> usize {
    let skeleton = EvidenceItem::CurrentSource(PreparedRange {
        resource: range.resource,
        path_rel: String::new(),
        resource_revision: range.resource_revision.clone(),
        span: range.span,
        source: String::new(),
        role: range.role,
        verification: SourceVerification {
            expected_content_hash: String::new(),
            observed_content_hash: String::new(),
            currentness: Currentness::Current,
        },
    });
    canonical::size(&skeleton) + (range.span.end_byte - range.span.start_byte)
}

fn check(
    continuation: &DeliveryContinuation,
    workspace: WorkspaceId,
    basis: &Basis,
    request_fingerprint: [u8; 32],
    projection_fingerprint: [u8; 32],
    budget: &DeliveryBudget,
) -> Result<(), DeliveryError> {
    let stable = basis
        .stable
        .as_ref()
        .map(|(no, revision)| (*no, revision.as_str()));
    let mismatch = if continuation.workspace != workspace {
        Some(ContinuationMismatch::Workspace)
    } else if continuation.index_incarnation != basis.incarnation {
        Some(ContinuationMismatch::IndexIncarnation)
    } else if basis.revision.as_deref() != Some(continuation.workspace_revision.as_str()) {
        Some(ContinuationMismatch::WorkspaceRevision)
    } else if stable
        != Some((
            continuation.generation_no,
            continuation.generation_basis_revision.as_str(),
        ))
    {
        Some(ContinuationMismatch::StableGeneration)
    } else if continuation.request_fingerprint != request_fingerprint {
        Some(ContinuationMismatch::Request)
    } else if continuation.projection_fingerprint != projection_fingerprint {
        Some(ContinuationMismatch::Projection)
    } else if continuation.budget != *budget {
        Some(ContinuationMismatch::Budget)
    } else {
        None
    };
    mismatch.map_or(Ok(()), |mismatch| {
        Err(DeliveryError::ContinuationMismatch(mismatch))
    })
}
