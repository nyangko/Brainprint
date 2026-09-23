//! #21 (I5 task 8) acceptance: actual raw/prepared/delivered stages, the
//! acknowledgement boundary, retained-context scopes, fact/source
//! invalidation, reuse inside the task 7 budget, and accounting. Same
//! fixtures as tasks 6/7; no backend installed or required.

use super::{
    delivery::{
        FixedTokens, broad_fixture, budget, chain, change_shared, flatten, is_optional_source,
        items, required_cost, understand_shared,
    },
    *,
};
use crate::{
    generation::GenerationStore,
    knowledge::{WorkItem, WorkItemStatus},
    projection::{ProjectionCorrelation, canonical},
    refresh::{RefreshOutcome, TargetedRefresh},
    watch::{RawWatchEvent, WatchIngest},
};

// ---------------------------------------------------------------- helpers

fn scoped(
    mut request: ProjectionRequest,
    client: Option<&str>,
    session: Option<&str>,
) -> ProjectionRequest {
    request.correlation = Some(ProjectionCorrelation {
        client_id: client.map(str::to_owned),
        session_id: session.map(str::to_owned),
        ..ProjectionCorrelation::default()
    });
    request
}

fn session(request: ProjectionRequest, session: &str) -> ProjectionRequest {
    scoped(request, Some("agent"), Some(session))
}

fn ledger() -> DeliveryLedger {
    DeliveryLedger::new(LedgerLimits::new(8, 1_000).expect("limits"))
}

fn pending_with(
    planner: &ProjectionPlanner,
    request: &ProjectionRequest,
    budget: &DeliveryBudget,
    tokens: Option<&dyn ExactTokenCounter>,
    ledger: &mut DeliveryLedger,
    retention: ContextRetention,
) -> PendingDelivery {
    let projection = planner.plan(request).expect("plan");
    planner
        .deliver_pending(
            request,
            &projection,
            budget,
            None,
            tokens,
            ledger,
            retention,
        )
        .expect("pending page")
}

fn pending(
    planner: &ProjectionPlanner,
    request: &ProjectionRequest,
    ledger: &mut DeliveryLedger,
    retention: ContextRetention,
) -> PendingDelivery {
    pending_with(planner, request, &items(10_000), None, ledger, retention)
}

/// Generate, then acknowledge: the economy as the caller ends up with it.
fn delivered(
    planner: &ProjectionPlanner,
    request: &ProjectionRequest,
    ledger: &mut DeliveryLedger,
    retention: ContextRetention,
) -> (PendingDelivery, ProjectionEconomy) {
    let pending = pending(planner, request, ledger, retention);
    let acknowledged = ledger.acknowledge(&pending.receipt);
    let mut economy = pending.economy.clone();
    economy.acknowledged(&acknowledged);
    (pending, economy)
}

const RETAINED: ContextRetention = ContextRetention::RetainedContext;
const FRESH: ContextRetention = ContextRetention::FreshContext;

fn hits(pending: &PendingDelivery) -> usize {
    pending.receipt.reuse.reuse_hits
}

/// Whether the page carries an item matching `pick` as a reference.
fn referenced(pending: &PendingDelivery, pick: impl Fn(&EvidenceItem) -> bool) -> bool {
    pending
        .page
        .evidence
        .iter()
        .zip(&pending.page.references)
        .any(|(item, reference)| pick(item) && reference.is_some())
}

/// Whether the page carries an item matching `pick` in full.
fn full(pending: &PendingDelivery, pick: impl Fn(&EvidenceItem) -> bool) -> bool {
    pending
        .page
        .evidence
        .iter()
        .zip(&pending.page.references)
        .any(|(item, reference)| pick(item) && reference.is_none())
}

fn is_policy(uid: brainprint_core::PolicyId) -> impl Fn(&EvidenceItem) -> bool {
    move |item| matches!(item, EvidenceItem::Policy(entry) if entry.item.uid == uid)
}

fn is_source_of(resource: ResourceId) -> impl Fn(&EvidenceItem) -> bool {
    move |item| matches!(item, EvidenceItem::CurrentSource(range) if range.resource == resource)
}

/// Canonical bytes of the page as represented.
fn represented_bytes(pending: &PendingDelivery) -> usize {
    let page = &pending.page;
    page.evidence
        .iter()
        .zip(&page.references)
        .map(|(item, reference)| {
            reference
                .as_ref()
                .map_or_else(|| canonical::size(item), canonical::size)
        })
        .chain(page.gaps.iter().map(canonical::size))
        .sum()
}

// ----------------------------------------------------------------- telemetry

#[test]
fn raw_available_is_what_the_query_surfaces_returned() {
    let fixture = Fixture::standard("raw");
    let planner = fixture.planner();

    // LOCATE: the two actual candidates, bytes exact from memory.
    let target = SymbolTarget::new(SymbolName::Name("run".to_owned()));
    let located = planner
        .index()
        .search_symbols(&target.query())
        .expect("query");
    let projection = planner
        .plan(&fixture.request(
            ProjectionIntent::Locate,
            Some(ProjectionTarget::Symbol(target)),
        ))
        .expect("plan");
    assert_eq!(projection.raw.items, Measure::Known(2));
    assert_eq!(
        projection.raw.bytes,
        Measure::Known(located.candidates.iter().map(canonical::size).sum())
    );
    assert_eq!(projection.raw.tokens, Measure::Unknown, "no exact counter");

    // Nothing returned is a measured zero, not an unknown.
    let none = planner
        .plan(&fixture.request(
            ProjectionIntent::Locate,
            Some(ProjectionTarget::Resource(ResourceTarget::Path(
                "src/missing.ts".to_owned(),
            ))),
        ))
        .expect("plan");
    assert_eq!(none.raw.items, Measure::Known(0));
    assert_eq!(none.raw.bytes, Measure::Known(0));
    assert_ne!(Measure::Known(0), Measure::Unknown);

    // UNDERSTAND: selection + both direct answers + range candidates
    // before dedupe/merge, from the surfaces themselves.
    let shared = fixture.endpoint("src/shared.ts", "shared");
    let relations = planner.tests.traversal().relations();
    let outgoing = relations.outgoing(&shared, &[]).expect("outgoing");
    let incoming = relations.incoming(&shared, &[]).expect("incoming");
    let spans: usize = outgoing
        .confirmed
        .iter()
        .chain(&incoming.confirmed)
        .map(|relation| relation.evidence.len())
        .sum();
    let expected = 1
        + outgoing.confirmed.len()
        + outgoing.gaps.len()
        + incoming.confirmed.len()
        + incoming.gaps.len()
        + 1
        + spans;
    let before = planner.stats();
    let projection = planner.plan(&understand_shared(&fixture)).expect("plan");
    let after = planner.stats();
    assert_eq!(projection.raw.items, Measure::Known(expected));
    assert_eq!(
        projection.raw.bytes,
        Measure::Unknown,
        "range bodies are not read to size them"
    );
    assert_eq!(projection.raw.tokens, Measure::Unknown);
    assert_eq!(
        after.relation_queries - before.relation_queries,
        2,
        "no extra query"
    );
    assert_eq!(after.impact_traversals, before.impact_traversals);
    assert_eq!(after.source_file_reads - before.source_file_reads, 1);
}

#[test]
fn prepared_counts_the_deduped_merged_universe_once() {
    let fixture = Fixture::standard("prepared");
    let planner = fixture.planner();
    let mut ledger = ledger();

    let request = understand_shared(&fixture);
    let projection = planner.plan(&request).expect("plan");
    let optional = projection
        .source_plan
        .iter()
        .filter(|range| range.requirement == SourceRequirement::Optional)
        .count();
    assert_eq!(optional, 4);
    let pending = planner
        .deliver_pending(
            &request,
            &projection,
            &items(10_000),
            None,
            None,
            &mut ledger,
            RETAINED,
        )
        .expect("pending");
    // The required declaration range is its CurrentSource, not a second unit.
    assert_eq!(
        pending.economy.prepared.items,
        Measure::Known(projection.evidence.len() + projection.gaps.len() + optional)
    );
    assert_eq!(
        pending.economy.prepared.bytes,
        Measure::Unknown,
        "unread optional bodies"
    );
    assert_eq!(pending.economy.prepared.tokens, Measure::Unknown);

    // No optional source: prepared bytes are exact.
    let locate = fixture.request(
        ProjectionIntent::Locate,
        symbol(SymbolName::Name("shared".to_owned())),
    );
    let projection = planner.plan(&locate).expect("plan");
    let pending = planner
        .deliver_pending(
            &locate,
            &projection,
            &items(10_000),
            None,
            Some(&FixedTokens(7)),
            &mut ledger,
            RETAINED,
        )
        .expect("pending");
    let bytes: usize = projection
        .evidence
        .iter()
        .map(canonical::size)
        .chain(projection.gaps.iter().map(canonical::size))
        .sum();
    assert_eq!(pending.economy.prepared.bytes, Measure::Known(bytes));
    assert_eq!(
        pending.economy.prepared.tokens,
        Measure::Known(7 * (projection.evidence.len() + projection.gaps.len()))
    );
}

#[test]
fn delivered_is_not_measured_until_acknowledged() {
    let fixture = Fixture::standard("ack-boundary");
    let planner = fixture.planner();
    let mut ledger = ledger();
    let request = session(change_shared(&fixture), "s1");

    let pending = pending(&planner, &request, &mut ledger, RETAINED);
    assert_eq!(pending.economy.delivered, StageAmount::NOT_MEASURED);
    assert_eq!(pending.economy.reuse, None);
    assert_eq!(pending.economy.fallback, FallbackObservation::NotObserved);
    assert_eq!(
        pending.economy.delivery_ratio(DeliveryDimension::Items),
        None
    );

    let acknowledged = ledger.acknowledge(&pending.receipt);
    let mut economy = pending.economy.clone();
    economy.acknowledged(&acknowledged);
    assert_eq!(
        economy.delivered.items,
        Measure::Known(pending.page.used_items)
    );
    assert_eq!(
        economy.delivered.bytes,
        Measure::Known(pending.page.used_bytes)
    );
    assert_eq!(economy.delivered.tokens, Measure::Unknown, "no counter");
    assert_eq!(economy.reuse, Some(pending.receipt.reuse));

    // With an exact counter the delivered tokens are exact too.
    let counter = FixedTokens(7);
    let pending = pending_with(
        &planner,
        &request,
        &items(10_000),
        Some(&counter),
        &mut ledger,
        ContextRetention::ReuseDisabled,
    );
    let acknowledged = ledger.acknowledge(&pending.receipt);
    assert_eq!(
        acknowledged.delivered.tokens,
        Measure::Known(7 * pending.page.used_items)
    );
}

#[test]
fn ratios_exist_only_for_exact_inputs() {
    let fixture = Fixture::standard("ratios");
    let planner = fixture.planner();
    let mut ledger = ledger();
    let locate = fixture.request(
        ProjectionIntent::Locate,
        symbol(SymbolName::Name("run".to_owned())),
    );
    let (_, economy) = delivered(&planner, &locate, &mut ledger, RETAINED);
    for dimension in [DeliveryDimension::Items, DeliveryDimension::Bytes] {
        assert!(economy.preparation_ratio(dimension).is_some());
        assert!(economy.delivery_ratio(dimension).is_some());
    }
    assert_eq!(economy.delivery_ratio(DeliveryDimension::Tokens), None);
    assert!(
        economy
            .budget_utilization(DeliveryDimension::Items)
            .is_some()
    );
    assert_eq!(
        economy.budget_utilization(DeliveryDimension::Bytes),
        None,
        "no cap"
    );

    let (_, economy) = delivered(
        &planner,
        &understand_shared(&fixture),
        &mut ledger,
        RETAINED,
    );
    assert_eq!(
        economy.preparation_ratio(DeliveryDimension::Bytes),
        None,
        "raw bytes unknown"
    );
    assert!(
        economy
            .preparation_ratio(DeliveryDimension::Items)
            .is_some()
    );
}

#[test]
fn telemetry_and_reuse_add_no_work() {
    let (fixture, target) = broad_fixture("economy-broad");
    let request = scoped(
        fixture.request(
            ProjectionIntent::Change(Some(ChangeKind::Structural(ImpactIntent::Rename))),
            Some(ProjectionTarget::Endpoint(target)),
        ),
        Some("agent"),
        Some("s1"),
    );
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let (required, _) = required_cost(&planner, &request, &projection);
    let everything = planner
        .deliver(&request, &projection, &items(10_000), None, None)
        .expect("page");
    let facts = everything
        .evidence
        .iter()
        .position(is_optional_source)
        .expect("sources last")
        + everything.gaps.len();
    assert!(facts > required);

    // Excluded optional source stays unread with telemetry and reuse on.
    let mut ledger = ledger();
    let small = items(facts);
    let before = planner.stats();
    let plain = planner
        .deliver(&request, &projection, &small, None, None)
        .expect("page");
    let middle = planner.stats();
    let pending = planner
        .deliver_pending(
            &request,
            &projection,
            &small,
            None,
            None,
            &mut ledger,
            RETAINED,
        )
        .expect("pending");
    let after = planner.stats();
    assert_eq!(pending.page, plain, "the same task 7 page");
    for (first, second) in [
        (
            middle.source_file_reads - before.source_file_reads,
            after.source_file_reads - middle.source_file_reads,
        ),
        (
            middle.relation_queries - before.relation_queries,
            after.relation_queries - middle.relation_queries,
        ),
        (
            middle.impact_traversals - before.impact_traversals,
            after.impact_traversals - middle.impact_traversals,
        ),
        (
            middle.optional_source_selected - before.optional_source_selected,
            after.optional_source_selected - middle.optional_source_selected,
        ),
    ] {
        assert_eq!((first, second), (0, 0));
    }
    assert!(pending.page.more_available);
}

#[test]
fn the_same_truth_observes_the_same_stages_whatever_the_ledger() {
    let fixture = Fixture::standard("same-truth");
    let planner = fixture.planner();
    let mut ledger = ledger();
    let request = session(change_shared(&fixture), "s1");

    let (first, first_economy) = delivered(&planner, &request, &mut ledger, RETAINED);
    let (second, second_economy) = delivered(&planner, &request, &mut ledger, RETAINED);
    assert_eq!(first.receipt.reuse.reuse_hits, 0, "empty ledger");
    assert!(hits(&second) > 0, "acknowledged payloads are referenced");
    assert_eq!(first_economy.raw_available, second_economy.raw_available);
    assert_eq!(first_economy.prepared, second_economy.prepared);
    // Truth is unchanged; only the representation differs.
    assert_eq!(first.page.evidence, second.page.evidence);
    assert!(second.page.used_bytes < first.page.used_bytes);
    assert_eq!(first.page.used_items, second.page.used_items);
}

// ---------------------------------------------- scope / acknowledgement

#[test]
fn without_a_client_or_session_reuse_is_disabled() {
    let fixture = Fixture::standard("no-scope");
    let planner = fixture.planner();
    let mut ledger = ledger();
    let request = scoped(change_shared(&fixture), None, None);
    let (first, _) = delivered(&planner, &request, &mut ledger, RETAINED);
    let (second, economy) = delivered(&planner, &request, &mut ledger, RETAINED);
    assert_eq!(first.receipt.scope, None);
    assert_eq!(hits(&second), 0);
    assert!(second.page.references.iter().all(Option::is_none));
    assert_eq!(
        economy.reuse.map(|reuse| reuse.reusable_candidates),
        Some(0)
    );
    assert_eq!(
        second.page,
        planner
            .deliver(
                &request,
                &planner.plan(&request).expect("plan"),
                &items(10_000),
                None,
                None
            )
            .expect("page")
    );
}

#[test]
fn only_acknowledged_payloads_become_references() {
    let fixture = Fixture::standard("ack");
    let planner = fixture.planner();
    let mut ledger = ledger();
    let request = session(change_shared(&fixture), "s1");

    let first = pending(&planner, &request, &mut ledger, RETAINED);
    assert_eq!(hits(&first), 0);
    let unacknowledged = pending(&planner, &request, &mut ledger, RETAINED);
    assert_eq!(hits(&unacknowledged), 0, "generated is not delivered");
    ledger.acknowledge(&unacknowledged.receipt);

    let second = pending(&planner, &request, &mut ledger, RETAINED);
    let reuse = second.receipt.reuse;
    assert!(reuse.reuse_hits > 0);
    assert_eq!(
        reuse.reuse_hits + reuse.reuse_misses,
        reuse.reusable_candidates
    );
    let replaced: usize = second
        .page
        .evidence
        .iter()
        .zip(&second.page.references)
        .filter(|(_, reference)| reference.is_some())
        .map(|(item, _)| canonical::size(item))
        .sum();
    let reference_bytes: usize = second
        .page
        .references
        .iter()
        .flatten()
        .map(canonical::size)
        .sum();
    assert_eq!(reuse.full_payload_bytes_replaced, replaced);
    assert_eq!(reuse.reuse_reference_bytes, reference_bytes);
    assert_eq!(
        reuse.bytes_not_retransmitted,
        replaced - reference_bytes,
        "actual representation delta"
    );
    assert_eq!(second.page.used_bytes, represented_bytes(&second));
    assert_eq!(
        second.page.used_bytes + reuse.bytes_not_retransmitted,
        first.page.used_bytes
    );
    assert_eq!(
        reuse.full_payload_tokens_replaced,
        Measure::Unknown,
        "no counter"
    );
    assert_eq!(reuse.reuse_reference_tokens, Measure::Unknown);
    // A reference is still one delivered item.
    assert_eq!(second.page.used_items, first.page.used_items);
    for reference in second.page.references.iter().flatten() {
        assert_eq!(
            canonical::size(reference),
            canonical::encode(reference).len()
        );
    }
}

#[test]
fn other_sessions_clients_and_workspaces_start_full() {
    let fixture = Fixture::standard("scopes");
    let planner = fixture.planner();
    let mut ledger = ledger();
    let request = change_shared(&fixture);
    delivered(
        &planner,
        &scoped(request.clone(), Some("agent"), Some("s1")),
        &mut ledger,
        RETAINED,
    );
    for other in [
        scoped(request.clone(), Some("agent"), Some("s2")),
        scoped(request.clone(), Some("other-agent"), Some("s1")),
        scoped(request.clone(), Some("agent"), None),
    ] {
        assert_eq!(
            hits(&pending(&planner, &other, &mut ledger, RETAINED)),
            0,
            "{:?}",
            other.correlation
        );
    }

    // Same client/session strings in another Workspace share nothing.
    let elsewhere = Fixture::standard("scopes-elsewhere");
    let there = session(change_shared(&elsewhere), "s1");
    assert_eq!(
        hits(&pending(
            &elsewhere.planner(),
            &there,
            &mut ledger,
            RETAINED
        )),
        0
    );
    assert!(
        hits(&pending(
            &planner,
            &session(request, "s1"),
            &mut ledger,
            RETAINED
        )) > 0
    );
}

#[test]
fn fresh_context_resets_only_its_own_scope() {
    let fixture = Fixture::standard("fresh");
    let protected = fixture
        .project()
        .insert_policy(&NewPolicy {
            protection_class: ProtectionClass::ProtectedSecurity,
            ..policy("never log secrets", "secrets")
        })
        .expect("policy");
    let planner = fixture.planner();
    let mut ledger = ledger();
    let request = change_shared(&fixture);
    let one = session(request.clone(), "s1");
    let two = session(request, "s2");
    delivered(&planner, &one, &mut ledger, RETAINED);
    delivered(&planner, &two, &mut ledger, RETAINED);
    assert!(referenced(
        &pending(&planner, &one, &mut ledger, RETAINED),
        is_policy(protected.uid)
    ));

    let fresh = pending(&planner, &one, &mut ledger, FRESH);
    assert_eq!(hits(&fresh), 0, "full payload");
    assert!(full(&fresh, is_policy(protected.uid)));
    assert_eq!(ledger.len(&fresh.receipt.scope.clone().expect("scope")), 0);
    assert!(
        hits(&pending(&planner, &two, &mut ledger, RETAINED)) > 0,
        "another session keeps its ledger"
    );
    // The fresh page, once acknowledged, builds a fresh ledger.
    ledger.acknowledge(&fresh.receipt);
    assert!(hits(&pending(&planner, &one, &mut ledger, RETAINED)) > 0);
}

#[test]
fn a_fresh_context_cannot_continue_a_page_chain() {
    let fixture = Fixture::standard("fresh-continuation");
    let planner = fixture.planner();
    let mut ledger = ledger();
    let request = session(change_shared(&fixture), "s1");
    let projection = planner.plan(&request).expect("plan");
    let (required, _) = required_cost(&planner, &request, &projection);
    let small = items(required + 1);
    let first = planner
        .deliver_pending(
            &request,
            &projection,
            &small,
            None,
            None,
            &mut ledger,
            RETAINED,
        )
        .expect("pending");
    let continuation = first.page.continuation.expect("more remains");
    assert!(matches!(
        planner.deliver_pending(
            &request,
            &projection,
            &small,
            Some(&continuation),
            None,
            &mut ledger,
            FRESH
        ),
        Err(DeliveryError::FreshContextWithContinuation)
    ));
    assert!(
        planner
            .deliver_pending(
                &request,
                &projection,
                &small,
                Some(&continuation),
                None,
                &mut ledger,
                RETAINED
            )
            .is_ok()
    );
}

#[test]
fn ledger_loss_and_eviction_only_cost_a_full_resend() {
    let fixture = Fixture::standard("loss");
    let planner = fixture.planner();
    let request = change_shared(&fixture);
    let one = session(request.clone(), "s1");
    let mut kept = ledger();
    delivered(&planner, &one, &mut kept, RETAINED);
    let reused = pending(&planner, &one, &mut kept, RETAINED);
    assert!(hits(&reused) > 0);

    // A lost ledger: full payloads, the same truth.
    let mut lost = ledger();
    let resent = pending(&planner, &one, &mut lost, RETAINED);
    assert_eq!(hits(&resent), 0);
    assert_eq!(resent.page.evidence, reused.page.evidence);

    // One scope at a time: the second session evicts the first.
    let mut small = DeliveryLedger::new(LedgerLimits::new(1, 1_000).expect("limits"));
    delivered(&planner, &one, &mut small, RETAINED);
    delivered(
        &planner,
        &session(request.clone(), "s2"),
        &mut small,
        RETAINED,
    );
    let evicted = pending(&planner, &one, &mut small, RETAINED);
    assert_eq!(hits(&evicted), 0);
    assert_eq!(evicted.page.evidence, reused.page.evidence);

    // Two records per scope, oldest out first.
    let mut tiny = DeliveryLedger::new(LedgerLimits::new(8, 2).expect("limits"));
    let first = delivered(&planner, &one, &mut tiny, RETAINED).0;
    let scope = first.receipt.scope.clone().expect("scope");
    assert!(first.receipt.full_payloads.len() > 2);
    assert_eq!(tiny.len(&scope), 2);
    let again = pending(&planner, &one, &mut tiny, RETAINED);
    assert!(hits(&again) <= 2);
    assert_eq!(again.page.evidence, reused.page.evidence);
}

#[test]
fn ledger_limits_are_explicit_and_nonzero() {
    assert_eq!(
        LedgerLimits::new(0, 5),
        Err(LedgerLimitsError::ZeroMaxScopes)
    );
    assert_eq!(
        LedgerLimits::new(5, 0),
        Err(LedgerLimitsError::ZeroMaxRecordsPerScope)
    );
    let code = include_str!("../economy.rs");
    assert!(!code.contains("impl Default for LedgerLimits"));
    assert!(!code.contains("impl Default for DeliveryLedger"));
    let derive = code
        .lines()
        .take_while(|line| !line.starts_with("pub struct LedgerLimits"))
        .last()
        .expect("derive line");
    assert!(!derive.contains("Default"), "{derive}");
}

// --------------------------------------------- fact / source invalidation

#[test]
fn knowledge_and_working_state_reuse_until_they_change() {
    let fixture = Fixture::standard("facts");
    let project = fixture.project();
    let planner = fixture.planner();
    let mut ledger = ledger();
    let request = session(change_shared(&fixture), "s1");
    let (first, _) = delivered(&planner, &request, &mut ledger, RETAINED);
    let policy_id = first
        .page
        .evidence
        .iter()
        .find_map(|item| match item {
            EvidenceItem::Policy(entry) => Some(entry.item.uid),
            _ => None,
        })
        .expect("policy");
    let decision_id = first
        .page
        .evidence
        .iter()
        .find_map(|item| match item {
            EvidenceItem::Decision(entry) => Some(entry.item.uid),
            _ => None,
        })
        .expect("decision");
    let is_decision = |uid: brainprint_core::DecisionId| move |item: &EvidenceItem| matches!(item, EvidenceItem::Decision(entry) if entry.item.uid == uid);
    let is_working = |item: &EvidenceItem| matches!(item, EvidenceItem::WorkingState(_));
    let is_work_item = |item: &EvidenceItem| matches!(item, EvidenceItem::WorkItem(_));
    let target_source = |item: &EvidenceItem| matches!(item, EvidenceItem::CurrentSource(range) if range.role == RangeRole::AnchorDeclaration);

    let second = pending(&planner, &request, &mut ledger, RETAINED);
    for pick in [
        &is_policy(policy_id) as &dyn Fn(&EvidenceItem) -> bool,
        &is_decision(decision_id),
        &is_working,
        &is_work_item,
        &target_source,
    ] {
        assert!(referenced(&second, pick));
    }

    // An unrelated Policy and an unrelated WorkItem invalidate nothing.
    let added = project
        .insert_policy(&policy("unrelated rule", "unrelated"))
        .expect("policy");
    fixture.work_item(
        "someone else",
        &[("src/unique.ts", WorkResourceRole::Target)],
    );
    let third = delivered(&planner, &request, &mut ledger, RETAINED).0;
    assert!(full(&third, is_policy(added.uid)), "new is full");
    for pick in [
        &is_policy(policy_id) as &dyn Fn(&EvidenceItem) -> bool,
        &is_decision(decision_id),
        &is_working,
        &target_source,
    ] {
        assert!(referenced(&third, pick));
    }

    // A changed Policy / Decision / Working State is sent in full.
    let replacement = project
        .insert_policy(&policy("no default exports, ever", "exports"))
        .expect("policy");
    project
        .supersede_policy(replacement.uid, policy_id)
        .expect("supersede");
    let new_decision = project
        .insert_decision(&decision("orm", "sqlx"))
        .expect("decision");
    project
        .supersede_decision(new_decision.uid, decision_id)
        .expect("supersede");
    fixture
        .work()
        .update_progress(
            request.work_item.expect("explicit"),
            &WorkProgress {
                current_step: Some("halfway".to_owned()),
                ..WorkProgress::default()
            },
            &[],
        )
        .expect("progress");
    let fourth = pending(&planner, &request, &mut ledger, RETAINED);
    assert!(!fourth.page.evidence.iter().any(is_policy(policy_id)));
    assert!(full(&fourth, is_policy(replacement.uid)));
    assert!(full(&fourth, is_decision(new_decision.uid)));
    assert!(full(&fourth, is_working), "same identity, new version");
    assert!(referenced(&fourth, is_policy(added.uid)));
    assert!(referenced(&fourth, target_source));
}

#[test]
fn source_reuse_follows_current_verification() {
    let fixture = Fixture::standard("source-reuse");
    let planner = fixture.planner();
    let mut ledger = ledger();
    let request = session(understand_shared(&fixture), "s1");
    let shared = fixture.resource("src/shared.ts").id;
    let other = fixture.resource("src/other.ts").id;
    delivered(&planner, &request, &mut ledger, RETAINED);

    // Every source is still read and verified; then it is referenced.
    let before = planner.stats();
    let second = delivered(&planner, &request, &mut ledger, RETAINED).0;
    let after = planner.stats();
    assert_eq!(
        after.source_file_reads - before.source_file_reads,
        4,
        "shared + 3 optional files, read again"
    );
    assert!(referenced(&second, is_source_of(shared)));
    assert!(referenced(&second, is_source_of(other)));

    // Changed before refresh: no reference, no stale text, full corrective.
    fs::write(
        fixture.root.join("src/other.ts"),
        OTHER_TS.replace("shared()", "shared( )"),
    )
    .expect("edit");
    let third = delivered(&planner, &request, &mut ledger, RETAINED).0;
    assert!(!third.page.evidence.iter().any(is_source_of(other)));
    assert!(full(&third, |item| matches!(
        item,
        EvidenceItem::SourceUnavailable { resource, .. } if *resource == other
    )));
    assert!(referenced(&third, is_source_of(shared)));
}

#[test]
fn a_new_source_revision_is_delivered_in_full() {
    let fixture = Fixture::standard("new-revision");
    let mut ledger = ledger();
    let request = session(
        fixture.request(
            ProjectionIntent::Understand,
            symbol(SymbolName::QualifiedName("shared".to_owned())),
        ),
        "s1",
    );
    let shared = fixture.resource("src/shared.ts");
    let old_revision = shared.resource_revision.clone();
    let is_target = move |item: &EvidenceItem| {
        matches!(item, EvidenceItem::CurrentSource(range)
            if range.resource == shared.id && range.role == RangeRole::AnchorDeclaration)
    };
    delivered(&fixture.planner(), &request, &mut ledger, RETAINED);
    assert!(referenced(
        &pending(&fixture.planner(), &request, &mut ledger, RETAINED),
        is_target
    ));

    fs::write(
        fixture.root.join("src/shared.ts"),
        SHARED_TS.replace("return 1", "return 10"),
    )
    .expect("edit");
    // The planner's verified read notices the change first.
    let noticed = pending(&fixture.planner(), &request, &mut ledger, RETAINED);
    assert!(!noticed.page.evidence.iter().any(is_target));
    // The watcher sees the save, then the targeted refresh publishes it.
    let watch = WatchIngest::open(&fixture.paths.index_db).expect("index.db");
    watch.mark_watcher_continuous().expect("continuity");
    watch
        .ingest_all(
            &fixture.root,
            &WorkspaceConfig::default(),
            &[RawWatchEvent::Modified {
                path: fixture.root.join("src/shared.ts"),
            }],
        )
        .expect("ingest");
    let outcome = TargetedRefresh::open(&fixture.paths.index_db)
        .expect("index.db")
        .run(&fixture.root, &WorkspaceConfig::default())
        .expect("refresh");
    assert!(
        matches!(outcome, RefreshOutcome::Published(_)),
        "{outcome:?}"
    );
    assert_ne!(
        fixture.resource("src/shared.ts").resource_revision,
        old_revision
    );

    let fresh_revision = pending(&fixture.planner(), &request, &mut ledger, RETAINED);
    let source = fresh_revision
        .page
        .evidence
        .iter()
        .zip(&fresh_revision.page.references)
        .find(|(item, _)| matches!(item, EvidenceItem::CurrentSource(range) if range.role == RangeRole::AnchorDeclaration))
        .expect("the new declaration source");
    let EvidenceItem::CurrentSource(range) = source.0 else {
        unreachable!()
    };
    assert!(range.source.contains("return 10"));
    assert!(source.1.is_none(), "new revision is sent in full");
    ledger.acknowledge(&fresh_revision.receipt);
    assert!(referenced(
        &pending(&fixture.planner(), &request, &mut ledger, RETAINED),
        |item| item == source.0
    ));
}

#[test]
fn an_unrelated_generation_does_not_invalidate_unchanged_payloads() {
    let fixture = Fixture::standard("generation");
    let planner = fixture.planner();
    let mut ledger = ledger();
    let request = session(change_shared(&fixture), "s1");
    delivered(&planner, &request, &mut ledger, RETAINED);
    let before = pending(&planner, &request, &mut ledger, RETAINED);
    let stable = |fixture: &Fixture| {
        GenerationStore::open(&fixture.paths.index_db)
            .expect("index.db")
            .current_stable()
            .expect("stable")
            .expect("published")
            .generation_no
    };
    let generation = stable(&fixture);
    fixture.publish(&fixture.standard_graph());
    assert!(stable(&fixture) > generation, "a new generation");
    let after = pending(&planner, &request, &mut ledger, RETAINED);
    assert_eq!(hits(&after), hits(&before));
    assert_eq!(after.page.evidence, before.page.evidence);
}

#[test]
fn corrective_and_request_local_evidence_is_always_full() {
    let fixture = Fixture::standard("always-full");
    let project = fixture.project();
    project
        .insert_policy(&policy("tabs or spaces", "style"))
        .expect("policy");
    let planner = fixture.planner();
    let mut ledger = ledger();
    let mut request = session(
        fixture.request(
            ProjectionIntent::Change(None),
            Some(ProjectionTarget::Endpoint(
                fixture.endpoint("src/app.ts", "run"),
            )),
        ),
        "s1",
    );
    request.directives = vec![RequestDirective {
        id: "d1".to_owned(),
        target: DirectiveTarget::Policy,
        subject_key: "style".to_owned(),
        scope: KnowledgeScope::project(),
        summary: "spaces".to_owned(),
    }];
    delivered(&planner, &request, &mut ledger, RETAINED);
    let second = pending(&planner, &request, &mut ledger, RETAINED);
    assert!(hits(&second) > 0);
    for (item, reference) in second.page.evidence.iter().zip(&second.page.references) {
        if ReuseIdentity::of(item).is_none() {
            assert!(reference.is_none(), "{item:?}");
        }
    }
    for pick in [
        (|item: &EvidenceItem| matches!(item, EvidenceItem::Coverage(_)))
            as fn(&EvidenceItem) -> bool,
        |item| matches!(item, EvidenceItem::Directive(_)),
        |item| matches!(item, EvidenceItem::IndexCurrentness { .. }),
    ] {
        assert!(full(&second, pick));
    }
    assert_eq!(second.page.gaps, planner.plan(&request).expect("plan").gaps);
}

#[test]
fn a_retained_protected_policy_and_source_may_satisfy_the_bundle_by_reference() {
    let fixture = Fixture::standard("required-reference");
    let protected = fixture
        .project()
        .insert_policy(&NewPolicy {
            protection_class: ProtectionClass::ProtectedSecurity,
            rule_text: "never log secrets. ".repeat(100),
            ..policy("never log secrets", "secrets")
        })
        .expect("policy");
    let planner = fixture.planner();
    let mut ledger = ledger();
    let request = session(
        fixture.request(
            ProjectionIntent::Change(None),
            Some(ProjectionTarget::Endpoint(
                fixture.endpoint("src/app.ts", "run"),
            )),
        ),
        "s1",
    );
    let projection = planner.plan(&request).expect("plan");
    let (_, full_required) = required_cost(&planner, &request, &projection);
    delivered(&planner, &request, &mut ledger, RETAINED);

    // The bundle as references: find its size, then fit exactly that.
    let referenced_required = match planner.deliver_pending(
        &request,
        &projection,
        &budget(None, Some(1), None),
        None,
        None,
        &mut ledger,
        RETAINED,
    ) {
        Err(DeliveryError::BudgetTooSmallForRequiredEvidence { required_bytes, .. }) => {
            required_bytes
        }
        other => panic!("{other:?}"),
    };
    assert!(referenced_required < full_required);
    let small = budget(None, Some(referenced_required), None);
    let page = planner
        .deliver_pending(
            &request,
            &projection,
            &small,
            None,
            None,
            &mut ledger,
            RETAINED,
        )
        .expect("fits by reference");
    assert!(referenced(&page, is_policy(protected.uid)));
    assert!(referenced(&page, |item| matches!(
        item,
        EvidenceItem::CurrentSource(range) if range.role == RangeRole::AnchorDeclaration
    )));
    // Without the retained context the same cap cannot carry the bundle.
    assert!(matches!(
        planner.deliver(&request, &projection, &small, None, None),
        Err(DeliveryError::BudgetTooSmallForRequiredEvidence { .. })
    ));
    assert!(matches!(
        planner.deliver_pending(
            &request,
            &projection,
            &small,
            None,
            None,
            &mut ledger,
            FRESH
        ),
        Err(DeliveryError::BudgetTooSmallForRequiredEvidence { .. })
    ));
}

// ------------------------------------------------------------ budget/reuse

#[test]
fn a_reference_no_smaller_than_its_payload_is_not_used() {
    let fixture = Fixture::standard("not-smaller");
    let planner = fixture.planner();
    let mut ledger = ledger();
    let request = session(change_shared(&fixture), "s1");
    let mut projection = planner.plan(&request).expect("plan");
    // A WorkItem with empty text is smaller than any reference to it.
    let tiny = EvidenceItem::WorkItem(WorkItem {
        uid: WorkItemId::generate(),
        source_kind: WorkItemSourceKind::Issue,
        source_ref: None,
        title: None,
        goal: String::new(),
        status: WorkItemStatus::Active,
        created_at: String::new(),
        closed_at: None,
    });
    let reference = ReuseReference {
        identity: ReuseIdentity::of(&tiny).expect("reusable kind"),
        version: [0; 32],
    };
    assert!(canonical::size(&reference) >= canonical::size(&tiny));
    projection.evidence.push(tiny.clone());
    projection.delivery.push(DeliveryHint {
        relevance: Relevance::WorkingState,
        impact_depth: None,
    });
    let deliver = |ledger: &mut DeliveryLedger| {
        planner
            .deliver_pending(
                &request,
                &projection,
                &items(10_000),
                None,
                None,
                ledger,
                RETAINED,
            )
            .expect("pending")
    };
    let first = deliver(&mut ledger);
    ledger.acknowledge(&first.receipt);
    let second = deliver(&mut ledger);
    assert!(hits(&second) > 0);
    assert!(full(&second, |item| *item == tiny));
}

/// Fixed-cost exact counter that also knows a reference's cost.
struct ReferenceTokens;

impl ExactTokenCounter for ReferenceTokens {
    fn exact_tokens(&self, _: DeliveryUnit<'_>) -> Option<usize> {
        Some(7)
    }

    fn exact_planned_source_tokens(&self, _: &PlannedSourceRange) -> Option<usize> {
        Some(7)
    }

    fn exact_reuse_tokens(&self, _: &ReuseReference) -> Option<usize> {
        Some(1)
    }
}

#[test]
fn under_a_token_cap_a_reference_needs_an_exact_token_cost() {
    let fixture = Fixture::standard("reuse-tokens");
    let planner = fixture.planner();
    let mut ledger = ledger();
    let request = session(change_shared(&fixture), "s1");
    delivered(&planner, &request, &mut ledger, RETAINED);
    let capped = budget(None, None, Some(10_000));

    // Reference cost unknown: never guessed, the full payload is used.
    let unknown = pending_with(
        &planner,
        &request,
        &capped,
        Some(&FixedTokens(7)),
        &mut ledger,
        RETAINED,
    );
    assert_eq!(hits(&unknown), 0);
    assert_eq!(
        unknown.page.used_tokens,
        TokenUsage::Known(7 * unknown.page.used_items)
    );

    // Exact reference cost: references, counted exactly.
    let exact = pending_with(
        &planner,
        &request,
        &capped,
        Some(&ReferenceTokens),
        &mut ledger,
        RETAINED,
    );
    let hit_count = hits(&exact);
    assert!(hit_count > 0);
    assert_eq!(
        exact.page.used_tokens,
        TokenUsage::Known(7 * (exact.page.used_items - hit_count) + hit_count)
    );
    assert_eq!(
        exact.receipt.reuse.full_payload_tokens_replaced,
        Measure::Known(7 * hit_count)
    );
    assert_eq!(
        exact.receipt.reuse.reuse_reference_tokens,
        Measure::Known(hit_count)
    );
}

#[test]
fn reuse_aware_pages_are_deterministic_and_keep_truth() {
    let fixture = Fixture::standard("reuse-determinism");
    let planner = fixture.planner();
    let request = session(change_shared(&fixture), "s1");
    let projection = planner.plan(&request).expect("plan");
    let (required, _) = required_cost(&planner, &request, &projection);
    let run = || {
        let mut ledger = ledger();
        delivered(&planner, &request, &mut ledger, RETAINED);
        let mut pages = Vec::new();
        let mut continuation = None;
        loop {
            let pending = planner
                .deliver_pending(
                    &request,
                    &projection,
                    &items(required + 2),
                    continuation.as_ref(),
                    None,
                    &mut ledger,
                    RETAINED,
                )
                .expect("page");
            // The economy reports exactly what task 7 cut.
            assert_eq!(pending.economy.omitted_items, pending.page.omitted_units);
            assert_eq!(pending.economy.more_available, pending.page.more_available);
            assert_eq!(pending.economy.limiting, pending.page.limiting);
            assert_eq!(
                pending.economy.continuation_available,
                pending.page.continuation.is_some()
            );
            assert_eq!(
                pending.economy.continuation_unavailable,
                pending.page.continuation_unavailable
            );
            continuation = pending.page.continuation.clone();
            pages.push(pending);
            if continuation.is_none() {
                break;
            }
        }
        pages
    };
    let first = run();
    assert_eq!(first, run());
    let plain = chain(&planner, &request, &projection, &items(required + 2), None);
    assert_eq!(
        flatten(&first.iter().map(|p| p.page.clone()).collect::<Vec<_>>()),
        flatten(&plain),
        "reuse never changes the evidence"
    );
}

// ------------------------------------------------------------- boundary

#[test]
fn economy_has_no_sql_persistence_backend_or_second_engine() {
    let code: String = include_str!("../economy.rs")
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for forbidden in [
        // SQL / persistence
        "rusqlite",
        "execute(",
        "SELECT",
        "CREATE ",
        "INSERT",
        "fs::",
        "File::",
        // backends
        "Launcher",
        "Supervisor",
        "lsp::",
        "SemanticIndex",
        "runtime::",
        // a second task 7 engine
        "fn fill",
        "fn required",
        "fn sequence",
        "source_floor",
        "BudgetTooSmallForRequiredEvidence",
        "read_ranges",
        // estimates / task 9
        "/ 4",
        "confidence",
        "fan_in",
    ] {
        assert!(!code.contains(forbidden), "{forbidden}");
    }
    let engine = include_str!("../delivery.rs");
    assert_eq!(engine.matches("fn fill(").count(), 1);
    assert_eq!(
        engine
            .matches("DeliveryError::BudgetTooSmallForRequiredEvidence {")
            .count(),
        1
    );
}

/// The observations recorded in #21 for task 8. Asserted, and printed.
#[test]
fn economy_instrumentation_is_recorded() {
    let fixture = Fixture::standard("economy-instrumentation");
    let planner = fixture.planner();
    let mut ledger = ledger();
    let request = session(change_shared(&fixture), "s1");
    let record = |label: &str,
                  request: &ProjectionRequest,
                  retention,
                  ledger: &mut DeliveryLedger| {
        let before = planner.stats();
        let (pending, economy) = delivered(&planner, request, ledger, retention);
        let after = planner.stats();
        let reuse = economy.reuse.expect("acknowledged");
        eprintln!(
            "TASK8 {label}: raw={:?} prepared={:?} delivered={:?} candidates={} hits={} misses={} \
             replaced_bytes={} reference_bytes={} not_retransmitted={} source_reads={} source_bytes={} \
             relation_queries={} impact_traversals={} more_available={} continuation={}",
            economy.raw_available,
            economy.prepared,
            economy.delivered,
            reuse.reusable_candidates,
            reuse.reuse_hits,
            reuse.reuse_misses,
            reuse.full_payload_bytes_replaced,
            reuse.reuse_reference_bytes,
            reuse.bytes_not_retransmitted,
            after.source_file_reads - before.source_file_reads,
            after.source_bytes - before.source_bytes,
            after.relation_queries - before.relation_queries,
            after.impact_traversals - before.impact_traversals,
            economy.more_available,
            economy.continuation_available,
        );
        assert!(after.impact_traversals - before.impact_traversals <= 1);
        pending
    };
    let first = record("first (empty ledger)", &request, RETAINED, &mut ledger);
    assert_eq!(hits(&first), 0);
    let second = record(
        "second same scope after ack",
        &request,
        RETAINED,
        &mut ledger,
    );
    assert!(hits(&second) > 0);
    let fresh = record("FreshContext", &request, FRESH, &mut ledger);
    assert_eq!(hits(&fresh), 0);
    let other = record(
        "different session",
        &session(request.clone(), "s2"),
        RETAINED,
        &mut ledger,
    );
    assert_eq!(hits(&other), 0);
    fixture.publish(&fixture.standard_graph());
    let generation = record("unrelated generation", &request, RETAINED, &mut ledger);
    assert!(hits(&generation) > 0);
    fs::write(
        fixture.root.join("src/shared.ts"),
        SHARED_TS.replace("return 1", "return 2"),
    )
    .expect("edit");
    let changed = record(
        "source changed before refresh",
        &request,
        RETAINED,
        &mut ledger,
    );
    assert!(
        changed
            .page
            .evidence
            .iter()
            .any(|item| matches!(item, EvidenceItem::SourceUnavailable { .. }))
    );
    let mut lost = self::ledger();
    let loss = record("ledger clear/loss", &request, RETAINED, &mut lost);
    assert_eq!(hits(&loss), 0);
}
