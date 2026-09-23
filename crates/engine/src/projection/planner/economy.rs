//! Output-economy observation + acknowledged delivery reuse (#21, I5
//! task 8).
//!
//! Three actual stages, never counterfactual costs: `raw_available` is
//! what the query surfaces returned for this request (counted by the
//! planner where they returned it), `prepared` is the task 6 universe the
//! task 7 sequence is cut from, and `delivered` is what an acknowledged
//! page carried. A generated page is only *pending*: nothing is delivered
//! or reusable until [`DeliveryLedger::acknowledge`] succeeds.
//!
//! The ledger is in-memory delivery metadata for one retained client or
//! session context -- identities and payload digests, never payloads and
//! never truth. Losing, clearing, or evicting it only makes the next page
//! carry full payloads. A reference is chosen by the task 7 engine itself
//! and only for an item this request already produced (a source only after
//! its verified read), so a hit can never stand in for current source.

use std::{
    collections::{BTreeMap, VecDeque},
    error::Error,
    fmt,
    num::NonZeroUsize,
};

use brainprint_core::{
    BlueprintApplicationId, DecisionId, PolicyId, ProjectStateId, ResourceId, SymbolId,
    UserPreferenceId, WorkItemId, WorkspaceId,
};

use super::{
    ContinuationUnavailable, DeliveryBudget, DeliveryContinuation, DeliveryDimension,
    DeliveryError, DeliveryPage, DeliveryUnit, ExactTokenCounter, PreparedProjection,
    ProjectionPlanner, delivery::ReuseLookup,
};
use crate::{
    graph::{GraphEndpoint, RelationKind},
    parser::SourceSpan,
    prepare::RangeRole,
    projection::{
        EvidenceItem, ProjectionRequest,
        canonical::{self, Canon, Canonical},
    },
};

const IDENTITY_DOMAIN: &str = "brainprint.reuse-identity.v1";
const PAYLOAD_DOMAIN: &str = "brainprint.delivery-payload.v1";
const PAGE_DOMAIN: &str = "brainprint.delivery-page.v1";

// ------------------------------------------------------------- measures

/// A fact-only amount: `Known(0)` is a measured zero, `Unknown` could not
/// be measured exactly, `NotMeasured` has not happened (yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Measure {
    Known(usize),
    Unknown,
    NotMeasured,
}

impl Measure {
    const fn known(self) -> Option<usize> {
        match self {
            Self::Known(value) => Some(value),
            Self::Unknown | Self::NotMeasured => None,
        }
    }

    fn of(value: Option<usize>) -> Self {
        value.map_or(Self::Unknown, Self::Known)
    }
}

/// Items / canonical bytes / exact tokens of one stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageAmount {
    pub items: Measure,
    pub bytes: Measure,
    pub tokens: Measure,
}

impl StageAmount {
    pub const NOT_MEASURED: Self = Self {
        items: Measure::NotMeasured,
        bytes: Measure::NotMeasured,
        tokens: Measure::NotMeasured,
    };

    #[must_use]
    pub const fn get(&self, dimension: DeliveryDimension) -> Measure {
        match dimension {
            DeliveryDimension::Items => self.items,
            DeliveryDimension::Bytes => self.bytes,
            DeliveryDimension::Tokens => self.tokens,
        }
    }
}

/// Whether a native fallback happened. Task 8 never runs one; only a
/// caller that observed it can say so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackObservation {
    NotObserved,
    NotUsed,
    Used(String),
}

// ---------------------------------------------------- scope / retention

/// Whose retained context a ledger describes. Same strings in another
/// Workspace are another scope.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeliveryScope {
    pub workspace: WorkspaceId,
    pub client_id: Option<String>,
    pub session_id: Option<String>,
}

impl DeliveryScope {
    /// The request's scope, or `None` (reuse disabled) when it names
    /// neither a client nor a session. Task/role/team/persona never count.
    #[must_use]
    pub fn of(request: &ProjectionRequest) -> Option<Self> {
        let correlation = request.correlation.as_ref()?;
        if correlation.client_id.is_none() && correlation.session_id.is_none() {
            return None;
        }
        Some(Self {
            workspace: request.workspace,
            client_id: correlation.client_id.clone(),
            session_id: correlation.session_id.clone(),
        })
    }
}

/// What the higher layer knows about the receiving context. Never
/// inferred here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextRetention {
    /// Earlier acknowledged payloads are still in the context.
    RetainedContext,
    /// Cleared, compacted, reconnected, or a fresh worker: the scope's
    /// ledger is dropped and everything is sent in full.
    FreshContext,
    /// No lookup, no recording.
    ReuseDisabled,
}

// ------------------------------------------------------------- identity

/// Semantic identity of a reusable payload; the version is separate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReuseIdentity {
    Resource(ResourceId),
    Symbol(SymbolId),
    Relation {
        kind: RelationKind,
        source: GraphEndpoint,
        target: GraphEndpoint,
    },
    Policy(PolicyId),
    Decision(DecisionId),
    Preference(UserPreferenceId),
    Blueprint(BlueprintApplicationId),
    ProjectState(ProjectStateId),
    WorkItem(WorkItemId),
    WorkingState(WorkItemId),
    WorkResult(WorkItemId),
    Handoff(WorkItemId),
    CurrentSource {
        resource: ResourceId,
        resource_revision: String,
        span: SourceSpan,
        role: RangeRole,
    },
    RelatedTest {
        target: GraphEndpoint,
        test: ResourceId,
    },
}

impl ReuseIdentity {
    /// `None` for evidence that is always delivered in full: gaps,
    /// coverage, selection, directives, conflicts, source unavailable,
    /// currentness, overlaps, generation references, staleness.
    #[must_use]
    pub fn of(item: &EvidenceItem) -> Option<Self> {
        Some(match item {
            EvidenceItem::Resource(resource) => Self::Resource(resource.id),
            EvidenceItem::Symbol(candidate) => Self::Symbol(candidate.symbol.id),
            EvidenceItem::Relation(relation) => Self::Relation {
                kind: relation.kind,
                source: relation.source.clone(),
                target: relation.target.clone(),
            },
            EvidenceItem::Policy(entry) => Self::Policy(entry.item.uid),
            EvidenceItem::Decision(entry) => Self::Decision(entry.item.uid),
            EvidenceItem::Preference(entry) => Self::Preference(entry.item.uid),
            EvidenceItem::Blueprint(entry) => Self::Blueprint(entry.item.application.uid),
            EvidenceItem::ProjectState(entry) => Self::ProjectState(entry.item.uid),
            EvidenceItem::WorkItem(item) => Self::WorkItem(item.uid),
            EvidenceItem::WorkingState(state) => Self::WorkingState(state.work_item),
            EvidenceItem::WorkResult(result) => Self::WorkResult(result.work_item),
            EvidenceItem::Handoff(handoff) => Self::Handoff(handoff.work_item),
            EvidenceItem::CurrentSource(range) => Self::CurrentSource {
                resource: range.resource,
                resource_revision: range.resource_revision.clone(),
                span: range.span,
                role: range.role,
            },
            EvidenceItem::RelatedTest { target, candidate } => Self::RelatedTest {
                target: target.clone(),
                test: candidate.resource,
            },
            EvidenceItem::RelationGap(_)
            | EvidenceItem::Directive(_)
            | EvidenceItem::KnowledgeConflict(_)
            | EvidenceItem::WorkOverlap { .. }
            | EvidenceItem::GenerationReference { .. }
            | EvidenceItem::WorkStaleness { .. }
            | EvidenceItem::TargetSelection(_)
            | EvidenceItem::Coverage(_)
            | EvidenceItem::SourceUnavailable { .. }
            | EvidenceItem::IndexCurrentness { .. } => return None,
        })
    }
}

impl Canonical for ReuseIdentity {
    fn encode(&self, out: &mut Canon) {
        match self {
            Self::Resource(id) => {
                out.tag(0);
                id.encode(out);
            }
            Self::Symbol(id) => {
                out.tag(1);
                id.encode(out);
            }
            Self::Relation {
                kind,
                source,
                target,
            } => {
                out.tag(2);
                kind.encode(out);
                source.encode(out);
                target.encode(out);
            }
            Self::Policy(id) => {
                out.tag(3);
                id.encode(out);
            }
            Self::Decision(id) => {
                out.tag(4);
                id.encode(out);
            }
            Self::Preference(id) => {
                out.tag(5);
                id.encode(out);
            }
            Self::Blueprint(id) => {
                out.tag(6);
                id.encode(out);
            }
            Self::ProjectState(id) => {
                out.tag(7);
                id.encode(out);
            }
            Self::WorkItem(id) => {
                out.tag(8);
                id.encode(out);
            }
            Self::WorkingState(id) => {
                out.tag(9);
                id.encode(out);
            }
            Self::WorkResult(id) => {
                out.tag(10);
                id.encode(out);
            }
            Self::Handoff(id) => {
                out.tag(11);
                id.encode(out);
            }
            Self::CurrentSource {
                resource,
                resource_revision,
                span,
                role,
            } => {
                out.tag(12);
                resource.encode(out);
                resource_revision.encode(out);
                span.encode(out);
                role.encode(out);
            }
            Self::RelatedTest { target, test } => {
                out.tag(13);
                target.encode(out);
                test.encode(out);
            }
        }
    }
}

/// A reference to a payload this scope already acknowledged, delivered in
/// its place. Delivery metadata, not truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReuseReference {
    pub identity: ReuseIdentity,
    /// SHA-256 of the full canonical payload.
    pub version: [u8; 32],
}

impl Canonical for ReuseReference {
    fn encode(&self, out: &mut Canon) {
        self.identity.encode(out);
        self.version.encode(out);
    }
}

fn version(item: &EvidenceItem) -> [u8; 32] {
    canonical::digest(PAYLOAD_DOMAIN, item)
}

// ---------------------------------------------------------------- ledger

/// Explicit ledger bounds; no `Default`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerLimits {
    max_scopes: NonZeroUsize,
    max_records_per_scope: NonZeroUsize,
}

impl LedgerLimits {
    pub fn new(max_scopes: usize, max_records_per_scope: usize) -> Result<Self, LedgerLimitsError> {
        Ok(Self {
            max_scopes: NonZeroUsize::new(max_scopes).ok_or(LedgerLimitsError::ZeroMaxScopes)?,
            max_records_per_scope: NonZeroUsize::new(max_records_per_scope)
                .ok_or(LedgerLimitsError::ZeroMaxRecordsPerScope)?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerLimitsError {
    ZeroMaxScopes,
    ZeroMaxRecordsPerScope,
}

impl fmt::Display for LedgerLimitsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ledger limit is zero: {self:?}")
    }
}

impl Error for LedgerLimitsError {}

/// One scope's acknowledged payload versions, oldest first.
#[derive(Default)]
struct ScopeRecords {
    records: BTreeMap<[u8; 32], ReuseReference>,
    order: VecDeque<[u8; 32]>,
}

/// In-memory, caller-owned retained-context ledger. FIFO-bounded per
/// scope and in number of scopes; eviction only means a full resend.
pub struct DeliveryLedger {
    limits: LedgerLimits,
    scopes: BTreeMap<DeliveryScope, ScopeRecords>,
    arrival: VecDeque<DeliveryScope>,
}

impl DeliveryLedger {
    #[must_use]
    pub const fn new(limits: LedgerLimits) -> Self {
        Self {
            limits,
            scopes: BTreeMap::new(),
            arrival: VecDeque::new(),
        }
    }

    /// Records held for `scope`.
    #[must_use]
    pub fn len(&self, scope: &DeliveryScope) -> usize {
        self.scopes
            .get(scope)
            .map_or(0, |records| records.order.len())
    }

    /// A reference for `item` when `scope` acknowledged this exact
    /// identity at this exact version.
    fn lookup(&self, scope: &DeliveryScope, item: &EvidenceItem) -> Option<ReuseReference> {
        let identity = ReuseIdentity::of(item)?;
        let found = self
            .scopes
            .get(scope)?
            .records
            .get(&canonical::digest(IDENTITY_DOMAIN, &identity))?;
        (found.version == version(item)).then(|| found.clone())
    }

    fn clear(&mut self, scope: &DeliveryScope) {
        if self.scopes.remove(scope).is_some() {
            self.arrival.retain(|held| held != scope);
        }
    }

    fn record(&mut self, scope: &DeliveryScope, reference: &ReuseReference) {
        if !self.scopes.contains_key(scope) {
            if self.scopes.len() == self.limits.max_scopes.get()
                && let Some(oldest) = self.arrival.pop_front()
            {
                self.scopes.remove(&oldest);
            }
            self.scopes.insert(scope.clone(), ScopeRecords::default());
            self.arrival.push_back(scope.clone());
        }
        let records = self.scopes.get_mut(scope).expect("inserted above");
        let key = canonical::digest(IDENTITY_DOMAIN, &reference.identity);
        if records.records.insert(key, reference.clone()).is_some() {
            records.order.retain(|held| *held != key);
        }
        records.order.push_back(key);
        if records.order.len() > self.limits.max_records_per_scope.get()
            && let Some(oldest) = records.order.pop_front()
        {
            records.records.remove(&oldest);
        }
    }

    /// The transport delivered the page this receipt describes: finalize
    /// its delivered observation and record its full reusable payloads.
    pub fn acknowledge(&mut self, receipt: &DeliveryReceipt) -> Acknowledged {
        if let Some(scope) = &receipt.scope {
            for reference in &receipt.full_payloads {
                self.record(scope, reference);
            }
        }
        Acknowledged {
            delivered: StageAmount {
                items: Measure::Known(receipt.items),
                bytes: Measure::Known(receipt.bytes),
                tokens: Measure::of(receipt.tokens),
            },
            reuse: receipt.reuse,
        }
    }
}

// --------------------------------------------------------------- receipt

/// Actual reuse facts of one page, from its real representation sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReuseObservation {
    /// Page items with a reusable identity, when a scope was looked up.
    pub reusable_candidates: usize,
    pub reuse_hits: usize,
    pub reuse_misses: usize,
    pub full_payload_bytes_replaced: usize,
    pub reuse_reference_bytes: usize,
    pub bytes_not_retransmitted: usize,
    /// Only when every replaced payload and reference was counted exactly.
    pub full_payload_tokens_replaced: Measure,
    pub reuse_reference_tokens: Measure,
}

/// Compact metadata for [`DeliveryLedger::acknowledge`]: no payload, no
/// source text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryReceipt {
    /// `None`: nothing is recorded on acknowledgement.
    pub scope: Option<DeliveryScope>,
    pub page_fingerprint: [u8; 32],
    /// Identity + version of each reusable item the page carried in full.
    pub full_payloads: Vec<ReuseReference>,
    /// The references the page carried instead of payloads.
    pub references: Vec<ReuseReference>,
    pub items: usize,
    pub bytes: usize,
    pub tokens: Option<usize>,
    pub reuse: ReuseObservation,
}

/// What acknowledgement finalizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Acknowledged {
    pub delivered: StageAmount,
    pub reuse: ReuseObservation,
}

// --------------------------------------------------------------- economy

/// One request/page observation. Engine-internal; not a history.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionEconomy {
    pub raw_available: StageAmount,
    pub prepared: StageAmount,
    /// `NOT_MEASURED` until acknowledged.
    pub delivered: StageAmount,
    pub budget: DeliveryBudget,
    pub omitted_items: usize,
    pub more_available: bool,
    pub limiting: std::collections::BTreeSet<DeliveryDimension>,
    pub continuation_available: bool,
    pub continuation_unavailable: Option<ContinuationUnavailable>,
    /// `None` until acknowledged.
    pub reuse: Option<ReuseObservation>,
    pub fallback: FallbackObservation,
}

impl ProjectionEconomy {
    pub fn acknowledged(&mut self, acknowledged: &Acknowledged) {
        self.delivered = acknowledged.delivered;
        self.reuse = Some(acknowledged.reuse);
    }

    /// prepared / raw_available, from exact inputs only.
    #[must_use]
    pub fn preparation_ratio(&self, dimension: DeliveryDimension) -> Option<f64> {
        ratio(
            self.prepared.get(dimension),
            self.raw_available.get(dimension),
        )
    }

    /// delivered / prepared, from exact inputs only.
    #[must_use]
    pub fn delivery_ratio(&self, dimension: DeliveryDimension) -> Option<f64> {
        ratio(self.delivered.get(dimension), self.prepared.get(dimension))
    }

    /// delivered / the page cap, when that dimension has one.
    #[must_use]
    pub fn budget_utilization(&self, dimension: DeliveryDimension) -> Option<f64> {
        let cap = match dimension {
            DeliveryDimension::Items => self.budget.max_items(),
            DeliveryDimension::Bytes => self.budget.max_bytes(),
            DeliveryDimension::Tokens => self.budget.max_tokens(),
        }?;
        ratio(self.delivered.get(dimension), Measure::Known(cap.get()))
    }
}

#[allow(clippy::cast_precision_loss)]
fn ratio(numerator: Measure, denominator: Measure) -> Option<f64> {
    let denominator = denominator.known().filter(|value| *value > 0)?;
    Some(numerator.known()? as f64 / denominator as f64)
}

/// A generated page that is not delivered yet.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingDelivery {
    pub page: DeliveryPage,
    pub economy: ProjectionEconomy,
    pub receipt: DeliveryReceipt,
}

struct PageContent<'a>(&'a DeliveryPage);

impl Canonical for PageContent<'_> {
    fn encode(&self, out: &mut Canon) {
        self.0.evidence.encode(out);
        self.0.references.encode(out);
        self.0.gaps.encode(out);
    }
}

impl ProjectionPlanner {
    /// One task 7 page with retained-context reuse and its economy, as a
    /// pending delivery. Nothing is delivered or recorded until the caller
    /// acknowledges its receipt on `ledger`.
    #[allow(clippy::too_many_arguments)]
    pub fn deliver_pending(
        &self,
        request: &ProjectionRequest,
        projection: &PreparedProjection,
        budget: &DeliveryBudget,
        continuation: Option<&DeliveryContinuation>,
        tokens: Option<&dyn ExactTokenCounter>,
        ledger: &mut DeliveryLedger,
        retention: ContextRetention,
    ) -> Result<PendingDelivery, DeliveryError> {
        if retention == ContextRetention::FreshContext && continuation.is_some() {
            return Err(DeliveryError::FreshContextWithContinuation);
        }
        let scope = match retention {
            ContextRetention::ReuseDisabled => None,
            ContextRetention::RetainedContext | ContextRetention::FreshContext => {
                DeliveryScope::of(request)
            }
        };
        if let (ContextRetention::FreshContext, Some(scope)) = (retention, &scope) {
            ledger.clear(scope);
        }

        let ledger: &DeliveryLedger = ledger;
        let lookup =
            |item: &EvidenceItem| scope.as_ref().and_then(|scope| ledger.lookup(scope, item));
        let reuse: Option<ReuseLookup<'_>> = scope.as_ref().map(|_| &lookup as ReuseLookup<'_>);
        let (page, sources) =
            self.deliver_with(request, projection, budget, continuation, tokens, reuse)?;

        // prepared: the deduped evidence, gaps, and merged optional source
        // candidates (a required range is already its CurrentSource).
        let exact = |unit| tokens.and_then(|counter| counter.exact_tokens(unit));
        let mut prepared_tokens = Some(0_usize);
        let mut prepared_bytes = 0;
        for item in &projection.evidence {
            prepared_bytes += canonical::size(item);
            prepared_tokens = prepared_tokens
                .zip(exact(DeliveryUnit::Evidence(item)))
                .map(sum);
        }
        for gap in &projection.gaps {
            prepared_bytes += canonical::size(gap);
            prepared_tokens = prepared_tokens.zip(exact(DeliveryUnit::Gap(gap))).map(sum);
        }
        for range in &sources {
            let planned = tokens.and_then(|counter| counter.exact_planned_source_tokens(range));
            prepared_tokens = prepared_tokens.zip(planned).map(sum);
        }
        let prepared = StageAmount {
            items: Measure::Known(
                projection.evidence.len() + projection.gaps.len() + sources.len(),
            ),
            // An unread optional body has no exact size yet.
            bytes: if sources.is_empty() {
                Measure::Known(prepared_bytes)
            } else {
                Measure::Unknown
            },
            tokens: if tokens.is_some() {
                Measure::of(prepared_tokens)
            } else {
                Measure::Unknown
            },
        };

        let receipt = receipt(scope, &page, tokens);
        let economy = ProjectionEconomy {
            raw_available: projection.raw,
            prepared,
            delivered: StageAmount::NOT_MEASURED,
            budget: *budget,
            omitted_items: page.omitted_units,
            more_available: page.more_available,
            limiting: page.limiting.clone(),
            continuation_available: page.continuation.is_some(),
            continuation_unavailable: page.continuation_unavailable,
            reuse: None,
            fallback: FallbackObservation::NotObserved,
        };
        Ok(PendingDelivery {
            page,
            economy,
            receipt,
        })
    }
}

const fn sum((a, b): (usize, usize)) -> usize {
    a + b
}

fn receipt(
    scope: Option<DeliveryScope>,
    page: &DeliveryPage,
    tokens: Option<&dyn ExactTokenCounter>,
) -> DeliveryReceipt {
    let mut full_payloads = Vec::new();
    let mut references = Vec::new();
    let mut observation = ReuseObservation {
        reusable_candidates: 0,
        reuse_hits: 0,
        reuse_misses: 0,
        full_payload_bytes_replaced: 0,
        reuse_reference_bytes: 0,
        bytes_not_retransmitted: 0,
        full_payload_tokens_replaced: Measure::Known(0),
        reuse_reference_tokens: Measure::Known(0),
    };
    let mut full_tokens = Some(0);
    let mut reference_tokens = Some(0);
    if scope.is_some() {
        for (item, reference) in page.evidence.iter().zip(&page.references) {
            let Some(identity) = ReuseIdentity::of(item) else {
                continue;
            };
            observation.reusable_candidates += 1;
            match reference {
                Some(reference) => {
                    observation.reuse_hits += 1;
                    observation.full_payload_bytes_replaced += canonical::size(item);
                    observation.reuse_reference_bytes += canonical::size(reference);
                    full_tokens =
                        full_tokens
                            .zip(tokens.and_then(|counter| {
                                counter.exact_tokens(DeliveryUnit::Evidence(item))
                            }))
                            .map(sum);
                    reference_tokens = reference_tokens
                        .zip(tokens.and_then(|counter| counter.exact_reuse_tokens(reference)))
                        .map(sum);
                    references.push(reference.clone());
                }
                None => {
                    observation.reuse_misses += 1;
                    full_payloads.push(ReuseReference {
                        identity,
                        version: version(item),
                    });
                }
            }
        }
    }
    observation.bytes_not_retransmitted =
        observation.full_payload_bytes_replaced - observation.reuse_reference_bytes;
    observation.full_payload_tokens_replaced = Measure::of(full_tokens);
    observation.reuse_reference_tokens = Measure::of(reference_tokens);
    DeliveryReceipt {
        scope,
        page_fingerprint: canonical::digest(PAGE_DOMAIN, &PageContent(page)),
        full_payloads,
        references,
        items: page.used_items,
        bytes: page.used_bytes,
        tokens: match page.used_tokens {
            super::TokenUsage::Known(tokens) => Some(tokens),
            super::TokenUsage::Unknown => None,
        },
        reuse: observation,
    }
}
