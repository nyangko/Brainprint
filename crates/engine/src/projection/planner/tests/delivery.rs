//! #20 task 7 acceptance: explicit hard budgets, the required integrity
//! bundle, priority, budget-before-read optional source, and continuation
//! bindings. Same fixtures as task 6; no backend installed or required.

use brainprint_core::IndexIncarnationId;

use super::*;
use crate::{generation::GenerationStore, parser::SourcePoint, projection::canonical};

// ---------------------------------------------------------------- helpers

fn budget(items: Option<usize>, bytes: Option<usize>, tokens: Option<usize>) -> DeliveryBudget {
    DeliveryBudget::new(items, bytes, tokens).expect("budget")
}

fn items(cap: usize) -> DeliveryBudget {
    budget(Some(cap), None, None)
}

/// A test tokenizer with an exact, fixed cost per unit.
struct FixedTokens(usize);

impl ExactTokenCounter for FixedTokens {
    fn exact_tokens(&self, _: DeliveryUnit<'_>) -> Option<usize> {
        Some(self.0)
    }
}

/// Knows nothing about source text.
struct NoSourceTokens;

impl ExactTokenCounter for NoSourceTokens {
    fn exact_tokens(&self, unit: DeliveryUnit<'_>) -> Option<usize> {
        match unit {
            DeliveryUnit::Evidence(EvidenceItem::CurrentSource(_)) => None,
            _ => Some(1),
        }
    }
}

fn first(
    planner: &ProjectionPlanner,
    request: &ProjectionRequest,
    projection: &PreparedProjection,
    budget: &DeliveryBudget,
) -> DeliveryPage {
    planner
        .deliver(request, projection, budget, None, None)
        .expect("first page")
}

fn next(
    planner: &ProjectionPlanner,
    request: &ProjectionRequest,
    projection: &PreparedProjection,
    budget: &DeliveryBudget,
    continuation: &DeliveryContinuation,
) -> Result<DeliveryPage, DeliveryError> {
    planner.deliver(request, projection, budget, Some(continuation), None)
}

/// Every page of one chain.
fn chain(
    planner: &ProjectionPlanner,
    request: &ProjectionRequest,
    projection: &PreparedProjection,
    budget: &DeliveryBudget,
    tokens: Option<&dyn ExactTokenCounter>,
) -> Vec<DeliveryPage> {
    let mut pages = vec![
        planner
            .deliver(request, projection, budget, None, tokens)
            .expect("first page"),
    ];
    while let Some(continuation) = pages.last().expect("a page").continuation.clone() {
        assert!(pages.len() < 500, "a chain ends");
        pages.push(
            planner
                .deliver(request, projection, budget, Some(&continuation), tokens)
                .expect("continuation page"),
        );
    }
    pages
}

fn flatten(pages: &[DeliveryPage]) -> Vec<EvidenceItem> {
    pages
        .iter()
        .flat_map(|page| page.evidence.iter().cloned())
        .collect()
}

/// The required bundle's exact cost, read from the explicit error.
fn required_cost(
    planner: &ProjectionPlanner,
    request: &ProjectionRequest,
    projection: &PreparedProjection,
) -> (usize, usize) {
    match planner.deliver(
        request,
        projection,
        &budget(None, Some(1), None),
        None,
        None,
    ) {
        Err(DeliveryError::BudgetTooSmallForRequiredEvidence {
            required_items,
            required_bytes,
            ..
        }) => (required_items, required_bytes),
        other => panic!("one byte fits no bundle: {other:?}"),
    }
}

fn page_bytes(page: &DeliveryPage) -> usize {
    page.evidence
        .iter()
        .map(canonical::size)
        .chain(page.gaps.iter().map(canonical::size))
        .sum()
}

fn is_optional_source(item: &EvidenceItem) -> bool {
    matches!(item, EvidenceItem::CurrentSource(range) if range.role == RangeRole::EvidenceSpan)
}

/// CHANGE(Rename) of `shared` with a Policy, a requested Decision and an
/// explicit WorkItem: every optional tier has something in it.
fn change_shared(fixture: &Fixture) -> ProjectionRequest {
    let project = fixture.project();
    project
        .insert_policy(&policy("no default exports", "exports"))
        .expect("policy");
    project
        .insert_decision(&decision("orm", "none"))
        .expect("decision");
    let mine = fixture.work_item(
        "rename shared",
        &[("src/shared.ts", WorkResourceRole::Target)],
    );
    let mut request = fixture.request(
        ProjectionIntent::Change(Some(ChangeKind::Structural(ImpactIntent::Rename))),
        Some(ProjectionTarget::Endpoint(
            fixture.endpoint("src/shared.ts", "shared"),
        )),
    );
    request.work_item = Some(mine);
    request.knowledge.decision_topics.insert("orm".to_owned());
    request
}

fn span(start_byte: usize, end_byte: usize) -> SourceSpan {
    let point = SourcePoint { line: 0, column: 0 };
    SourceSpan {
        start_byte,
        end_byte,
        start: point,
        end: point,
    }
}

// ----------------------------------------------------------------- budget

#[test]
fn a_budget_is_explicit_nonzero_and_has_no_default() {
    assert!(matches!(
        DeliveryBudget::new(None, None, None),
        Err(DeliveryError::NoBudgetCap)
    ));
    for (caps, dimension) in [
        ((Some(0), None, None), DeliveryDimension::Items),
        ((None, Some(0), None), DeliveryDimension::Bytes),
        ((Some(3), None, Some(0)), DeliveryDimension::Tokens),
    ] {
        assert!(matches!(
            DeliveryBudget::new(caps.0, caps.1, caps.2),
            Err(DeliveryError::ZeroBudgetCap(found)) if found == dimension
        ));
    }
    let code = include_str!("../delivery.rs");
    assert!(!code.contains("impl Default for DeliveryBudget"));
    let derive = code
        .lines()
        .take_while(|line| !line.starts_with("pub struct DeliveryBudget"))
        .last()
        .expect("derive line");
    assert!(!derive.contains("Default"), "{derive}");
}

#[test]
fn the_item_cap_is_exact_on_every_page() {
    let fixture = Fixture::standard("items");
    let request = change_shared(&fixture);
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let (required, _) = required_cost(&planner, &request, &projection);

    let cap = required + 2;
    let pages = chain(&planner, &request, &projection, &items(cap), None);
    assert!(pages.len() > 2, "{} pages", pages.len());
    for (index, page) in pages.iter().enumerate() {
        assert!(page.used_items <= cap);
        assert_eq!(page.used_items, page.evidence.len() + page.gaps.len());
        if page.more_available {
            assert_eq!(page.used_items, cap, "page {index} is full");
            assert_eq!(page.limiting, BTreeSet::from([DeliveryDimension::Items]));
        }
    }
}

#[test]
fn the_byte_cap_counts_exact_canonical_bytes() {
    let fixture = Fixture::standard("bytes");
    let request = change_shared(&fixture);
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let (_, required) = required_cost(&planner, &request, &projection);

    // The sizer and the encoder agree on every unit.
    for item in &projection.evidence {
        assert_eq!(canonical::size(item), canonical::encode(item).len());
    }

    let cap = required + 300;
    let pages = chain(
        &planner,
        &request,
        &projection,
        &budget(None, Some(cap), None),
        None,
    );
    assert!(pages.len() > 1);
    for page in &pages {
        assert_eq!(page.used_bytes, page_bytes(page));
        assert!(page.used_bytes <= cap, "{} > {cap}", page.used_bytes);
        assert_eq!(page.used_tokens, TokenUsage::Unknown, "no counter");
        if page.more_available {
            assert_eq!(page.limiting, BTreeSet::from([DeliveryDimension::Bytes]));
        }
    }
}

#[test]
fn an_exact_token_counter_enforces_a_hard_token_cap() {
    let fixture = Fixture::standard("tokens");
    let request = change_shared(&fixture);
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let (required, _) = required_cost(&planner, &request, &projection);

    let counter = FixedTokens(7);
    let cap = 7 * (required + 3);
    let pages = chain(
        &planner,
        &request,
        &projection,
        &budget(None, None, Some(cap)),
        Some(&counter),
    );
    assert!(pages.len() > 1);
    for page in &pages {
        let TokenUsage::Known(used) = page.used_tokens else {
            panic!("an exact counter was supplied");
        };
        assert_eq!(used, 7 * page.used_items);
        assert!(used <= cap);
        if page.more_available {
            assert_eq!(page.limiting, BTreeSet::from([DeliveryDimension::Tokens]));
        }
    }

    // A token cap is never estimated.
    assert!(matches!(
        planner.deliver(
            &request,
            &projection,
            &budget(None, None, Some(cap)),
            None,
            None
        ),
        Err(DeliveryError::TokenCounterRequired)
    ));
    // The required target source has no exact count under this counter.
    assert!(matches!(
        planner.deliver(
            &request,
            &projection,
            &budget(None, None, Some(cap)),
            None,
            Some(&NoSourceTokens)
        ),
        Err(DeliveryError::TokenCostUnknown)
    ));
}

#[test]
fn the_required_bundle_is_never_cut_to_fit() {
    let fixture = Fixture::standard("required");
    let project = fixture.project();
    let protected = project
        .insert_policy(&NewPolicy {
            protection_class: ProtectionClass::ProtectedSecurity,
            ..policy("never log secrets", "secrets")
        })
        .expect("policy");
    project
        .insert_policy(&policy("tabs or spaces", "style"))
        .expect("policy");
    let mut request = fixture.request(
        ProjectionIntent::Change(None),
        Some(ProjectionTarget::Endpoint(
            fixture.endpoint("src/app.ts", "run"),
        )),
    );
    request.directives = vec![RequestDirective {
        id: "d1".to_owned(),
        target: DirectiveTarget::Policy,
        subject_key: "style".to_owned(),
        scope: KnowledgeScope::project(),
        summary: "spaces".to_owned(),
    }];
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let target_source = sources(&projection)[0].clone();

    let (required_items, required_bytes) = required_cost(&planner, &request, &projection);
    // Exactly enough: the whole bundle, nothing cut.
    let page = first(
        &planner,
        &request,
        &projection,
        &budget(None, Some(required_bytes), None),
    );
    assert_eq!(page.used_bytes, required_bytes);
    assert_eq!(page.used_items, required_items);
    let has = |pick: &dyn Fn(&EvidenceItem) -> bool| page.evidence.iter().any(pick);
    assert!(has(&|item| matches!(item, EvidenceItem::Symbol(_))));
    assert!(has(
        &|item| matches!(item, EvidenceItem::Policy(entry) if entry.item.uid == protected.uid)
    ));
    assert!(has(&|item| matches!(item, EvidenceItem::Directive(_))));
    assert!(has(&|item| matches!(
        item,
        EvidenceItem::Coverage(coverage) if coverage.report.has(CoverageLimit::RequiresSemantics)
    )));
    assert!(page.gaps.contains(&ProjectionGap::RequiresSemantics));
    assert!(
        page.evidence
            .contains(&EvidenceItem::CurrentSource(target_source)),
        "the target source whole"
    );

    // One byte or one item short: explicit, never a trimmed bundle.
    for (small, dimension) in [
        (
            budget(None, Some(required_bytes - 1), None),
            DeliveryDimension::Bytes,
        ),
        (
            budget(Some(required_items - 1), None, None),
            DeliveryDimension::Items,
        ),
    ] {
        match planner.deliver(&request, &projection, &small, None, None) {
            Err(DeliveryError::BudgetTooSmallForRequiredEvidence {
                required_items: items,
                required_bytes: bytes,
                required_tokens,
                violated,
            }) => {
                assert_eq!((items, bytes), (required_items, required_bytes));
                assert_eq!(required_tokens, None, "no counter, no number");
                assert_eq!(violated, BTreeSet::from([dimension]));
            }
            other => panic!("{other:?}"),
        }
    }
}

// --------------------------------------------------- atomicity / priority

#[test]
fn relation_and_test_families_arrive_with_their_coverage() {
    let fixture = Fixture::standard("families");
    let request = change_shared(&fixture);
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let (required, _) = required_cost(&planner, &request, &projection);
    let pages = chain(&planner, &request, &projection, &items(required + 1), None);

    let head = &pages[0].evidence;
    let covered = |pick: fn(&CoverageSubject) -> bool| {
        head.iter()
            .any(|item| matches!(item, EvidenceItem::Coverage(coverage) if pick(&coverage.subject)))
    };
    assert!(covered(|subject| matches!(
        subject,
        CoverageSubject::Impact { .. }
    )));
    assert!(covered(|subject| matches!(
        subject,
        CoverageSubject::RelatedTests { .. }
    )));
    let all = flatten(&pages);
    assert!(
        all.iter()
            .any(|item| matches!(item, EvidenceItem::Relation(_)))
    );
    assert!(
        all.iter()
            .any(|item| matches!(item, EvidenceItem::RelatedTest { .. }))
    );
    // No coverage is ever split off onto a later page.
    for page in &pages[1..] {
        assert!(
            !page
                .evidence
                .iter()
                .any(|item| matches!(item, EvidenceItem::Coverage(_)))
        );
    }
}

fn chain_fixture(label: &str) -> Fixture {
    let fixture = Fixture::with_files(label, &[("src/chain.ts", CHAIN_TS)]);
    let calls = fixture.calls("src/chain.ts");
    let names = ["a", "b", "c", "d", "e"];
    let plan: Vec<FilePlan<'_>> = vec![(
        "src/chain.ts",
        (0..4)
            .map(|index| {
                evidence(
                    calls[index],
                    edge(
                        RelationKind::Calls,
                        &fixture.endpoint("src/chain.ts", names[index]),
                        &fixture.endpoint("src/chain.ts", names[index + 1]),
                    ),
                )
            })
            .collect(),
        Vec::new(),
    )];
    fixture.publish(&plan);
    fixture
}

#[test]
fn direct_then_rules_then_shallower_transitive_then_source() {
    let fixture = chain_fixture("priority");
    fixture
        .project()
        .insert_policy(&policy("keep chains short", "chains"))
        .expect("policy");
    let request = fixture.request(
        ProjectionIntent::Change(Some(ChangeKind::Structural(ImpactIntent::Rename))),
        Some(ProjectionTarget::Endpoint(
            fixture.endpoint("src/chain.ts", "e"),
        )),
    );
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let depth = |name: &str| {
        let source = fixture.endpoint("src/chain.ts", name);
        projection
            .evidence
            .iter()
            .zip(&projection.delivery)
            .find_map(|(item, hint)| match item {
                EvidenceItem::Relation(relation) if relation.source == source => {
                    Some(hint.impact_depth)
                }
                _ => None,
            })
            .expect("the edge is projected")
    };
    assert_eq!(
        (depth("d"), depth("c"), depth("b")),
        (Some(0), Some(1), Some(2))
    );

    let (required, _) = required_cost(&planner, &request, &projection);
    let all = flatten(&chain(
        &planner,
        &request,
        &projection,
        &items(required + 1),
        None,
    ));
    let position =
        |pick: &dyn Fn(&EvidenceItem) -> bool| all.iter().position(pick).expect("delivered");
    let edge_from = |name: &str| {
        let source = fixture.endpoint("src/chain.ts", name);
        position(
            &move |item| matches!(item, EvidenceItem::Relation(relation) if relation.source == source),
        )
    };
    let rule = position(&|item| matches!(item, EvidenceItem::Policy(_)));
    let first_source = position(&is_optional_source);
    assert!(edge_from("d") < rule, "direct before rules");
    assert!(rule < edge_from("c"), "rules before transitive");
    assert!(edge_from("c") < edge_from("b"), "shallower first");
    assert!(edge_from("b") < first_source, "facts before source bodies");
    // Optional source never displaces a higher-priority fact.
    assert!(
        all[first_source..]
            .iter()
            .all(|item| is_optional_source(item)
                || matches!(item, EvidenceItem::SourceUnavailable { .. }))
    );
}

#[test]
fn delivery_order_does_not_depend_on_row_insertion_order() {
    let fixture = Fixture::standard("delivery-order");
    let request = change_shared(&fixture);
    let run = |fixture: &Fixture| {
        let planner = fixture.planner();
        let projection = planner.plan(&request).expect("plan");
        let (required, _) = required_cost(&planner, &request, &projection);
        chain(&planner, &request, &projection, &items(required + 2), None)
            .into_iter()
            .map(|page| page.evidence)
            .collect::<Vec<_>>()
    };
    let before = run(&fixture);
    let mut reversed = fixture.standard_graph();
    reversed.reverse();
    fixture.publish(&reversed);
    assert_eq!(before, run(&fixture));
}

// ----------------------------------------------------------------- source

fn understand_shared(fixture: &Fixture) -> ProjectionRequest {
    fixture.request(
        ProjectionIntent::Understand,
        Some(ProjectionTarget::Endpoint(
            fixture.endpoint("src/shared.ts", "shared"),
        )),
    )
}

#[test]
fn optional_source_outside_the_budget_is_never_read() {
    let fixture = Fixture::standard("unread");
    let request = understand_shared(&fixture);
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let everything = first(&planner, &request, &projection, &items(10_000));
    let before_sources = everything
        .evidence
        .iter()
        .position(is_optional_source)
        .expect("optional source exists");

    let before = planner.stats();
    let page = first(&planner, &request, &projection, &items(before_sources));
    let after = planner.stats();
    assert!(page.more_available);
    assert_eq!(page.limiting, BTreeSet::from([DeliveryDimension::Items]));
    assert_eq!(after.source_file_reads, before.source_file_reads, "no read");
    assert_eq!(
        after.optional_source_selected,
        before.optional_source_selected
    );
    assert!(after.optional_source_candidates > before.optional_source_candidates);
}

#[test]
fn selected_optional_source_is_verified_and_read_once_per_file() {
    let fixture = Fixture::standard("read");
    let request = understand_shared(&fixture);
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let before = planner.stats();
    let page = first(&planner, &request, &projection, &items(10_000));
    let after = planner.stats();
    assert!(!page.more_available);
    assert!(page.continuation.is_none());

    let read: Vec<&PreparedRange> = page
        .evidence
        .iter()
        .filter_map(|item| match item {
            EvidenceItem::CurrentSource(range) if range.role == RangeRole::EvidenceSpan => {
                Some(range)
            }
            _ => None,
        })
        .collect();
    assert_eq!(read.len(), 4, "every call site of shared");
    let files: BTreeSet<ResourceId> = read.iter().map(|range| range.resource).collect();
    for range in &read {
        let text = fs::read_to_string(fixture.root.join(&range.path_rel)).expect("source");
        assert_eq!(
            range.source,
            text[range.span.start_byte..range.span.end_byte]
        );
        assert_eq!(
            range.verification.expected_content_hash,
            range.verification.observed_content_hash
        );
    }
    assert_eq!(
        after.source_file_reads - before.source_file_reads,
        files.len() as u64,
        "one verified read per file, app.ts's two sites included"
    );
    assert_eq!(
        after.optional_source_selected - before.optional_source_selected,
        4
    );
}

#[test]
fn a_changed_optional_source_becomes_source_unavailable() {
    let fixture = Fixture::standard("optional-changed");
    let request = understand_shared(&fixture);
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    fs::write(
        fixture.root.join("src/other.ts"),
        OTHER_TS.replace("shared()", "shared( )"),
    )
    .expect("edit");
    let other = fixture.resource("src/other.ts").id;

    let page = first(&planner, &request, &projection, &items(10_000));
    assert!(page.evidence.iter().any(|item| matches!(
        item,
        EvidenceItem::SourceUnavailable {
            resource,
            reason: SourceUnavailable::SourceChanged { .. },
            ..
        } if *resource == other
    )));
    assert!(
        !page.evidence.iter().any(
            |item| matches!(item, EvidenceItem::CurrentSource(range) if range.resource == other)
        ),
        "no stale text"
    );
    assert_eq!(
        page.used_bytes,
        page_bytes(&page),
        "replacement cost counted"
    );
}

#[test]
fn optional_ranges_dedupe_merge_and_stay_disjoint_before_read() {
    let fixture = Fixture::standard("merge");
    let request = fixture.request(
        ProjectionIntent::Impact(ChangeKind::Structural(ImpactIntent::Rename)),
        Some(ProjectionTarget::Endpoint(
            fixture.endpoint("src/shared.ts", "shared"),
        )),
    );
    let planner = fixture.planner();
    let mut projection = planner.plan(&request).expect("plan");
    let app = fixture.resource("src/app.ts");
    let other = fixture.resource("src/other.ts");
    let at = |text: &str, needle: &str| text.find(needle).expect("needle");
    let calls = at(APP_TS, "shared() + shared()");
    let run = at(APP_TS, "export function run");
    let range =
        |resource: &crate::resource::Resource, start: usize, end: usize| PlannedSourceRange {
            resource: resource.id,
            resource_revision: resource.resource_revision.clone(),
            span: span(start, end),
            role: RangeRole::EvidenceSpan,
            requirement: SourceRequirement::Optional,
        };
    projection.source_plan = vec![
        range(&app, calls, calls + 8),
        range(&app, calls, calls + 8),
        range(&app, calls + 4, calls + 19),
        range(&app, run, run + 6),
        range(&other, 0, 6),
    ];

    let before = planner.stats();
    let page = first(&planner, &request, &projection, &items(10_000));
    let after = planner.stats();
    let read: BTreeSet<(ResourceId, &str)> = page
        .evidence
        .iter()
        .filter_map(|item| match item {
            EvidenceItem::CurrentSource(range) => Some((range.resource, range.source.as_str())),
            _ => None,
        })
        .collect();
    // The duplicate collapsed, the overlap merged, the disjoint span and
    // the other file kept apart -- never a whole file.
    assert_eq!(
        read,
        BTreeSet::from([
            (app.id, "export"),
            (app.id, "shared() + shared()"),
            (other.id, "import"),
        ])
    );
    assert_eq!(after.source_file_reads - before.source_file_reads, 2);
    assert!(read.iter().all(|(_, text)| !text.contains("return 1")));
}

// ------------------------------------------------------------- pagination

#[test]
fn a_chain_covers_every_unit_once_and_ends_honestly() {
    let fixture = Fixture::standard("chain");
    let request = change_shared(&fixture);
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let (required, _) = required_cost(&planner, &request, &projection);
    let everything = first(&planner, &request, &projection, &items(10_000));
    assert!(!everything.more_available && everything.continuation.is_none());
    assert_eq!(everything.omitted_units, 0);
    assert!(everything.limiting.is_empty());

    let pages = chain(&planner, &request, &projection, &items(required + 3), None);
    // First page: the required bundle and every gap; later pages: only
    // new optional units.
    assert_eq!(pages[0].gaps, projection.gaps);
    for page in &pages[1..] {
        assert!(page.gaps.is_empty());
        assert!(
            !page
                .evidence
                .iter()
                .any(|item| matches!(item, EvidenceItem::Symbol(_) | EvidenceItem::Coverage(_)))
        );
        assert_eq!(page.target, projection.target, "the basis is kept");
    }
    for window in pages.windows(2) {
        assert!(window[0].more_available);
        assert_eq!(
            window[0].omitted_units,
            window[1].evidence.len() + window[1].omitted_units
        );
    }
    let last = pages.last().expect("a page");
    assert!(!last.more_available);
    assert!(last.continuation.is_none() && last.continuation_unavailable.is_none());
    assert_eq!(
        flatten(&pages),
        everything.evidence,
        "nothing repeated or lost"
    );
}

#[test]
fn the_same_state_request_and_budget_give_the_same_chain() {
    let fixture = Fixture::standard("stable-chain");
    let request = change_shared(&fixture);
    let run = || {
        let planner = fixture.planner();
        let projection = planner.plan(&request).expect("plan");
        let (required, _) = required_cost(&planner, &request, &projection);
        chain(&planner, &request, &projection, &items(required + 2), None)
    };
    assert_eq!(run(), run());
}

#[test]
fn a_continuation_holds_bindings_and_a_key_only() {
    let fixture = Fixture::standard("compact");
    let request = change_shared(&fixture);
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let (required, _) = required_cost(&planner, &request, &projection);
    let page = first(&planner, &request, &projection, &items(required + 1));
    let continuation = page.continuation.expect("more remains");

    let store = GenerationStore::open(&fixture.paths.index_db).expect("index.db");
    let stable = store.current_stable().expect("stable").expect("published");
    assert_eq!(continuation.workspace, fixture.workspace);
    assert_eq!(
        continuation.index_incarnation,
        store.index_incarnation_id().expect("incarnation")
    );
    assert_eq!(
        Some(continuation.workspace_revision.clone()),
        store.current_workspace_revision().expect("clock")
    );
    assert_eq!(continuation.generation_no, stable.generation_no);
    assert_eq!(
        continuation.generation_basis_revision,
        stable.basis_workspace_revision
    );
    // No payload: no source text, no evidence, no remaining-id list.
    let text = format!("{continuation:?}");
    assert!(!text.contains("function shared"), "{text}");
    assert!(!text.contains("no default exports"));
}

#[test]
fn every_binding_of_a_continuation_is_checked() {
    let fixture = Fixture::standard("bindings");
    let request = change_shared(&fixture);
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let (required, _) = required_cost(&planner, &request, &projection);
    let small = items(required + 1);
    let page = first(&planner, &request, &projection, &small);
    let good = page.continuation.expect("more remains");
    assert!(next(&planner, &request, &projection, &small, &good).is_ok());

    let tamper = |change: fn(&mut DeliveryContinuation)| {
        let mut continuation = good.clone();
        change(&mut continuation);
        next(&planner, &request, &projection, &small, &continuation)
    };
    for (change, expected) in [
        (
            (|c: &mut DeliveryContinuation| c.workspace = WorkspaceId::generate())
                as fn(&mut DeliveryContinuation),
            ContinuationMismatch::Workspace,
        ),
        (
            |c| c.index_incarnation = IndexIncarnationId::generate(),
            ContinuationMismatch::IndexIncarnation,
        ),
        (
            |c| c.workspace_revision = "elsewhere".to_owned(),
            ContinuationMismatch::WorkspaceRevision,
        ),
        (
            |c| c.generation_no += 1,
            ContinuationMismatch::StableGeneration,
        ),
        (
            |c| c.generation_basis_revision = "elsewhere".to_owned(),
            ContinuationMismatch::StableGeneration,
        ),
        (
            |c| c.request_fingerprint = [0; 32],
            ContinuationMismatch::Request,
        ),
        (
            |c| c.projection_fingerprint = [0; 32],
            ContinuationMismatch::Projection,
        ),
    ] {
        match tamper(change) {
            Err(DeliveryError::ContinuationMismatch(found)) => assert_eq!(found, expected),
            other => panic!("{expected:?}: {other:?}"),
        }
    }
    assert!(matches!(
        tamper(|c| c.next.identity = [0; 32]),
        Err(DeliveryError::InvalidContinuationCursor)
    ));
    // Another budget is another chain.
    assert!(matches!(
        next(&planner, &request, &projection, &items(required + 2), &good),
        Err(DeliveryError::ContinuationMismatch(
            ContinuationMismatch::Budget
        ))
    ));
}

#[test]
fn a_rebuilt_index_with_the_same_number_and_revision_rejects_the_continuation() {
    let fixture = Fixture::standard("rebuild-continuation");
    let request = fixture.request(
        ProjectionIntent::Impact(ChangeKind::Structural(ImpactIntent::Rename)),
        Some(ProjectionTarget::Endpoint(
            fixture.endpoint("src/shared.ts", "shared"),
        )),
    );
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let (required, _) = required_cost(&planner, &request, &projection);
    let small = items(required + 1);
    let continuation = first(&planner, &request, &projection, &small)
        .continuation
        .expect("more remains");
    drop(planner);

    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", fixture.paths.index_db.display()));
    }
    init_workspace(&fixture.root, &fixture.global).expect("re-init binds a new index.db");
    BaselineScan::open(&fixture.paths.index_db)
        .expect("index.db")
        .run_initial_scan(
            &fixture.root,
            &WorkspaceConfig::default(),
            "workspace-rev-1",
        )
        .expect("baseline scan");
    fixture.publish(&fixture.standard_graph());

    let store = GenerationStore::open(&fixture.paths.index_db).expect("index.db");
    let stable = store.current_stable().expect("stable").expect("published");
    assert_eq!(
        stable.generation_no, continuation.generation_no,
        "same number"
    );
    assert_eq!(
        stable.basis_workspace_revision,
        continuation.generation_basis_revision
    );
    assert_eq!(
        store.current_workspace_revision().expect("clock"),
        Some(continuation.workspace_revision.clone()),
        "same revision"
    );
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("re-plan");
    assert!(matches!(
        next(&planner, &request, &projection, &small, &continuation),
        Err(DeliveryError::ContinuationMismatch(
            ContinuationMismatch::IndexIncarnation
        ))
    ));
}

#[test]
fn a_new_workspace_revision_rejects_the_continuation() {
    let fixture = Fixture::standard("revision-continuation");
    let request = change_shared(&fixture);
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let (required, _) = required_cost(&planner, &request, &projection);
    let small = items(required + 1);
    let continuation = first(&planner, &request, &projection, &small)
        .continuation
        .expect("more remains");

    // The stable generation still points at revision 1.
    GenerationStore::open(&fixture.paths.index_db)
        .expect("index.db")
        .set_current_workspace_revision("workspace-rev-2")
        .expect("clock");
    assert!(matches!(
        next(&planner, &request, &projection, &small, &continuation),
        Err(DeliveryError::ContinuationMismatch(
            ContinuationMismatch::WorkspaceRevision
        ))
    ));
}

#[test]
fn a_changed_request_policy_or_working_state_rejects_the_continuation() {
    let fixture = Fixture::standard("truth-continuation");
    let request = change_shared(&fixture);
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let (required, _) = required_cost(&planner, &request, &projection);
    let small = items(required + 1);
    let continuation = first(&planner, &request, &projection, &small)
        .continuation
        .expect("more remains");

    // Another request, even one more knowledge ref.
    let mut other = request.clone();
    other.knowledge.state_keys.insert("phase".to_owned());
    let other_projection = planner.plan(&other).expect("plan");
    assert!(matches!(
        next(&planner, &other, &other_projection, &small, &continuation),
        Err(DeliveryError::ContinuationMismatch(
            ContinuationMismatch::Request
        ))
    ));

    // Same request, new Policy: the re-planned truth differs.
    fixture
        .project()
        .insert_policy(&policy("a new rule", "new"))
        .expect("policy");
    let replanned = planner.plan(&request).expect("re-plan");
    assert!(matches!(
        next(&planner, &request, &replanned, &small, &continuation),
        Err(DeliveryError::ContinuationMismatch(
            ContinuationMismatch::Projection
        ))
    ));

    // A fresh chain on the new truth, then Working State moves.
    let continuation = first(&planner, &request, &replanned, &small)
        .continuation
        .expect("more remains");
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
    let replanned_again = planner.plan(&request).expect("re-plan");
    assert!(matches!(
        next(&planner, &request, &replanned_again, &small, &continuation),
        Err(DeliveryError::ContinuationMismatch(
            ContinuationMismatch::Projection
        ))
    ));
}

#[test]
fn without_a_stable_generation_a_page_is_returned_but_no_continuation() {
    let fixture = Fixture::standard("no-stable");
    let request = change_shared(&fixture);
    rusqlite::Connection::open(&fixture.paths.index_db)
        .expect("raw")
        .execute(
            "UPDATE workspace_clock SET stable_generation_id = NULL WHERE id = 0",
            [],
        )
        .expect("clear the stable pointer");
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let (required, _) = required_cost(&planner, &request, &projection);
    let page = first(&planner, &request, &projection, &items(required + 1));
    assert!(page.more_available);
    assert_eq!(page.continuation, None);
    assert_eq!(
        page.continuation_unavailable,
        Some(ContinuationUnavailable::NoStableGeneration)
    );
}

#[test]
fn a_unit_larger_than_the_whole_budget_ends_the_chain_explicitly() {
    let fixture = Fixture::standard("never-fits");
    fixture
        .project()
        .insert_decision(&NewDecision {
            rationale: "why ".repeat(2_000),
            ..decision("orm", "none")
        })
        .expect("decision");
    let mut request = fixture.request(
        ProjectionIntent::Change(Some(ChangeKind::Structural(ImpactIntent::Rename))),
        Some(ProjectionTarget::Endpoint(
            fixture.endpoint("src/shared.ts", "shared"),
        )),
    );
    request.knowledge.decision_topics.insert("orm".to_owned());
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let (_, required) = required_cost(&planner, &request, &projection);
    // The bundle and more fit, the 8 KB Decision never does.
    let pages = chain(
        &planner,
        &request,
        &projection,
        &budget(None, Some(required + 1_000), None),
        None,
    );
    let last = pages.last().expect("a page");
    assert!(last.more_available, "not silently skipped");
    assert_eq!(last.continuation, None);
    assert_eq!(
        last.continuation_unavailable,
        Some(ContinuationUnavailable::UnitExceedsBudget)
    );
    assert!(
        !flatten(&pages)
            .iter()
            .any(|item| matches!(item, EvidenceItem::Decision(_)))
    );
}

// --------------------------------------------------------------- boundary

#[test]
fn evidence_truth_does_not_depend_on_the_budget() {
    let fixture = Fixture::standard("invariant");
    let request = change_shared(&fixture);
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let (required, required_bytes) = required_cost(&planner, &request, &projection);
    let whole =
        |budget: DeliveryBudget| flatten(&chain(&planner, &request, &projection, &budget, None));
    let reference = whole(items(10_000));
    for budget in [
        items(required + 1),
        items(required + 4),
        budget(None, Some(required_bytes + 600), None),
    ] {
        assert_eq!(whole(budget), reference, "{budget:?}");
    }
    for item in &reference {
        assert!(
            projection.evidence.contains(item) || is_optional_source(item),
            "every fact is the planner's own: {item:?}"
        );
    }
}

#[test]
fn a_broad_change_reads_only_what_its_budget_selected() {
    let hub = "export function hub(): number {\n  return 0\n}\n";
    let callers: Vec<(String, String)> = (0..40)
        .map(|index| {
            (
                format!("src/c{index:02}.ts"),
                format!(
                    "import {{ hub }} from './hub'\n\nexport function c{index:02}(): number {{\n  return hub()\n}}\n"
                ),
            )
        })
        .collect();
    let mut files: Vec<(&str, &str)> = vec![("src/hub.ts", hub)];
    files.extend(
        callers
            .iter()
            .map(|(rel, text)| (rel.as_str(), text.as_str())),
    );
    let fixture = Fixture::with_files("broad-delivery", &files);
    let target = fixture.endpoint("src/hub.ts", "hub");
    let plan: Vec<FilePlan<'_>> = callers
        .iter()
        .enumerate()
        .map(|(index, (rel, _))| {
            (
                rel.as_str(),
                vec![evidence(
                    fixture.calls(rel)[0],
                    edge(
                        RelationKind::Calls,
                        &fixture.endpoint(rel, &format!("c{index:02}")),
                        &target,
                    ),
                )],
                Vec::new(),
            )
        })
        .collect();
    fixture.publish(&plan);

    let request = fixture.request(
        ProjectionIntent::Change(Some(ChangeKind::Structural(ImpactIntent::Rename))),
        Some(ProjectionTarget::Endpoint(target)),
    );
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let everything = first(&planner, &request, &projection, &items(10_000));
    let facts = everything
        .evidence
        .iter()
        .position(is_optional_source)
        .expect("sources last");
    let bytes: usize = everything.evidence[..facts]
        .iter()
        .map(canonical::size)
        .sum::<usize>()
        + everything.gaps.iter().map(canonical::size).sum::<usize>();

    // Room for every fact and about three call-site bodies.
    let one_source = canonical::size(&everything.evidence[facts]);
    let before = planner.stats();
    let page = first(
        &planner,
        &request,
        &projection,
        &budget(None, Some(bytes + 3 * one_source + one_source / 2), None),
    );
    let after = planner.stats();
    let delivered = page
        .evidence
        .iter()
        .filter(|item| is_optional_source(item))
        .count();
    let selected = after.optional_source_selected - before.optional_source_selected;
    let reads = after.source_file_reads - before.source_file_reads;
    assert_eq!(
        after.optional_source_candidates - before.optional_source_candidates,
        40
    );
    eprintln!(
        "TASK7 broad CHANGE(40 callers): optional_source_candidates=40 \
         optional_source_selected={selected} source_files_read={reads} delivered_sources={delivered} \
         page_items={} canonical_page_bytes={} omitted_units={}",
        page.used_items, page.used_bytes, page.omitted_units
    );
    assert!(page.more_available);
    assert_eq!(delivered, 3);
    assert!(selected < 40, "{selected} selected");
    // Each caller is its own file: at most the one that no longer fit is
    // read beyond what was delivered.
    assert!(reads <= delivered as u64 + 1, "{reads} reads");
}

/// Delivery and canonical-encoding code, comments excluded.
fn delivery_code() -> String {
    [
        include_str!("../delivery.rs"),
        include_str!("../../canonical.rs"),
    ]
    .concat()
    .lines()
    .filter(|line| !line.trim_start().starts_with("//"))
    .collect::<Vec<_>>()
    .join("\n")
}

#[test]
fn delivery_has_no_ledger_schema_transport_or_backend() {
    let code = delivery_code();
    for forbidden in [
        // task 8
        "raw_available",
        "ledger",
        "reuse",
        // schema / persistence
        "rusqlite",
        "execute(",
        "CREATE ",
        "INSERT",
        "UPDATE ",
        // transport encodings
        "base64",
        "serde::",
        "Serialize",
        // semantic runtime
        "Launcher",
        "Supervisor",
        "runtime::",
        "lsp::",
        "SemanticIndex",
        // estimates
        "/ 4",
        "confidence",
    ] {
        assert!(!code.contains(forbidden), "{forbidden}");
    }
}

/// The observations recorded in #20 for task 7. Asserted, and printed.
#[test]
fn delivery_instrumentation_is_recorded() {
    let fixture = Fixture::standard("delivery-instrumentation");
    let request = change_shared(&fixture);
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");
    let (required, required_bytes) = required_cost(&planner, &request, &projection);
    let understand = understand_shared(&fixture);
    let understood = planner.plan(&understand).expect("plan");
    let counter = FixedTokens(7);
    for (label, request, projection, budget, tokens) in [
        (
            "CHANGE items=required+2",
            &request,
            &projection,
            items(required + 2),
            None,
        ),
        (
            "CHANGE bytes=required+600",
            &request,
            &projection,
            budget(None, Some(required_bytes + 600), None),
            None,
        ),
        (
            "CHANGE tokens=7*(required+3)",
            &request,
            &projection,
            budget(None, None, Some(7 * (required + 3))),
            Some(&counter as &dyn ExactTokenCounter),
        ),
        (
            "UNDERSTAND items=10000",
            &understand,
            &understood,
            items(10_000),
            None,
        ),
    ] {
        let before = planner.stats();
        let page = planner
            .deliver(request, projection, &budget, None, tokens)
            .expect("page");
        let after = planner.stats();
        eprintln!(
            "TASK7 {label}: optional_source_candidates={} optional_source_selected={} \
             source_files_read={} source_bytes_materialized={} page_items={} \
             canonical_page_bytes={} tokens={:?} more_available={} limiting={:?} omitted_units={}",
            after.optional_source_candidates - before.optional_source_candidates,
            after.optional_source_selected - before.optional_source_selected,
            after.source_file_reads - before.source_file_reads,
            after.source_bytes - before.source_bytes,
            page.used_items,
            page.used_bytes,
            page.used_tokens,
            page.more_available,
            page.limiting,
            page.omitted_units,
        );
        assert!(
            after.optional_source_selected - before.optional_source_selected
                <= after.optional_source_candidates - before.optional_source_candidates
        );
    }
}
