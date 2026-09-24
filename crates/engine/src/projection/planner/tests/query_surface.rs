//! #23 (I5 task 10) acceptance: the Core query surface over a real
//! registry-bound fixture (global / project / workspace / index DBs +
//! files). Same fixtures as tasks 6-8; no backend installed or required.

use std::{num::NonZeroUsize, path::Path};

use brainprint_core::{LogicalSymbolId, PolicyId};

use super::{
    delivery::{broad_fixture, budget, change_shared, items},
    *,
};
use crate::{
    boundary::{GroupingSpec, ResourceScope, StructuralSummaryIndex, StructuralSummaryRequest},
    graph::DomainEntity,
    knowledge::WorkItemStatus,
    logical_symbol::{self, LogicalIdentity},
    projection::ProjectionCorrelation,
    query::{Currentness, DEFAULT_CANDIDATE_LIMIT},
    query_surface::{
        ContextPurpose, ContextRequest, CoreError, CoreQuerySurface, DeliveryOptions, FindQuery,
        FindRequest, FindResult, ImpactRequest, InspectRequest, InvalidRequest, KnowledgeQuery,
        KnowledgeRequest, KnowledgeResult, LineageTarget, NotInitialized, OwnedTextPattern,
        ProjectedAnswer, QueryContext, RelationDirection, RelationsRequest, SurfaceStats,
        TargetResolution,
    },
    relations::RelationIndex,
    search::{QueryStatus, SearchBudget},
    symbol::SymbolKind,
};

// ---------------------------------------------------------------- helpers

const RETAINED: ContextRetention = ContextRetention::RetainedContext;
const FRESH: ContextRetention = ContextRetention::FreshContext;
const NO_REUSE: ContextRetention = ContextRetention::ReuseDisabled;

fn surface(fixture: &Fixture) -> CoreQuerySurface {
    CoreQuerySurface::open(&fixture.global.global_db, fixture.workspace).expect("surface")
}

fn ctx(fixture: &Fixture) -> QueryContext {
    QueryContext {
        workspace: fixture.workspace,
        correlation: None,
    }
}

fn session(fixture: &Fixture, session: &str) -> QueryContext {
    QueryContext {
        workspace: fixture.workspace,
        correlation: Some(ProjectionCorrelation {
            client_id: Some("agent".to_owned()),
            session_id: Some(session.to_owned()),
            ..ProjectionCorrelation::default()
        }),
    }
}

fn opts(budget: DeliveryBudget, retention: ContextRetention) -> DeliveryOptions<'static> {
    DeliveryOptions {
        budget,
        continuation: None,
        retention,
        tokens: None,
    }
}

fn wide() -> DeliveryOptions<'static> {
    opts(items(10_000), NO_REUSE)
}

fn ledger() -> DeliveryLedger {
    DeliveryLedger::new(LedgerLimits::new(8, 1_000).expect("limits"))
}

fn at(fixture: &Fixture, rel: &str, name: &str) -> ProjectionTarget {
    ProjectionTarget::Endpoint(fixture.endpoint(rel, name))
}

fn named(name: SymbolName) -> ProjectionTarget {
    ProjectionTarget::Symbol(SymbolTarget::new(name))
}

fn find_target(
    surface: &CoreQuerySurface,
    fixture: &Fixture,
    target: ProjectionTarget,
) -> ProjectedAnswer {
    match surface
        .find(
            FindRequest {
                context: ctx(fixture),
                query: FindQuery::Target {
                    target,
                    delivery: wide(),
                },
            },
            &mut ledger(),
        )
        .expect("find")
    {
        FindResult::Target(answer) => answer,
        other => panic!("a target answer: {other:?}"),
    }
}

fn inspect_with(
    surface: &CoreQuerySurface,
    context: QueryContext,
    target: ProjectionTarget,
    delivery: DeliveryOptions<'_>,
    ledger: &mut DeliveryLedger,
) -> Result<ProjectedAnswer, CoreError> {
    surface.inspect(
        InspectRequest {
            context,
            target,
            delivery,
        },
        ledger,
    )
}

fn inspect(
    surface: &CoreQuerySurface,
    fixture: &Fixture,
    target: ProjectionTarget,
) -> ProjectedAnswer {
    inspect_with(surface, ctx(fixture), target, wide(), &mut ledger()).expect("inspect")
}

fn impact(
    surface: &CoreQuerySurface,
    fixture: &Fixture,
    target: ProjectionTarget,
    change: ChangeKind,
) -> ProjectedAnswer {
    surface
        .impact(
            ImpactRequest {
                context: ctx(fixture),
                target,
                change,
                delivery: wide(),
            },
            &mut ledger(),
        )
        .expect("impact")
}

fn relations_of(
    surface: &CoreQuerySurface,
    fixture: &Fixture,
    target: ProjectionTarget,
    direction: RelationDirection,
    kinds: Vec<RelationKind>,
) -> crate::query_surface::RelationsResult {
    surface
        .relations(RelationsRequest {
            context: ctx(fixture),
            target,
            direction,
            kinds,
        })
        .expect("relations")
}

/// The facade form of a planner CHANGE / RESUME request.
fn context_request<'a>(
    context: QueryContext,
    request: &ProjectionRequest,
    delivery: DeliveryOptions<'a>,
) -> ContextRequest<'a> {
    let purpose = match request.intent {
        ProjectionIntent::Change(change) => ContextPurpose::Change {
            target: request.target.clone().expect("a target"),
            change,
            work_item: request.work_item,
        },
        ProjectionIntent::ResumeHandoff => ContextPurpose::Resume {
            work_item: request.work_item.expect("a WorkItem"),
            target: request.target.clone(),
        },
        other => panic!("not a context intent: {other:?}"),
    };
    ContextRequest {
        context,
        purpose,
        scope_layers: request.scope_layers.clone(),
        directives: request.directives.clone(),
        knowledge: request.knowledge.clone(),
        delivery,
    }
}

fn knowledge(
    surface: &CoreQuerySurface,
    fixture: &Fixture,
    query: KnowledgeQuery,
) -> KnowledgeResult {
    surface
        .knowledge(KnowledgeRequest {
            context: ctx(fixture),
            query,
        })
        .expect("knowledge")
}

fn page(answer: &ProjectedAnswer) -> &[EvidenceItem] {
    &answer.delivery.page.evidence
}

fn anchor_sources(answer: &ProjectedAnswer) -> Vec<&PreparedRange> {
    page(answer)
        .iter()
        .filter_map(|item| match item {
            EvidenceItem::CurrentSource(range) if range.role == RangeRole::AnchorDeclaration => {
                Some(range)
            }
            _ => None,
        })
        .collect()
}

fn any_source(answer: &ProjectedAnswer) -> bool {
    page(answer)
        .iter()
        .any(|item| matches!(item, EvidenceItem::CurrentSource(_)))
}

fn has(answer: &ProjectedAnswer, pick: impl Fn(&EvidenceItem) -> bool) -> bool {
    page(answer).iter().any(pick)
}

fn hits(answer: &ProjectedAnswer) -> usize {
    answer.delivery.receipt.reuse.reuse_hits
}

fn selection_of(items: &[EvidenceItem]) -> &TargetSelection {
    items
        .iter()
        .find_map(|item| match item {
            EvidenceItem::TargetSelection(selection) => Some(selection),
            _ => None,
        })
        .expect("a target selection")
}

fn sorted_debug<T: fmt::Debug>(values: impl IntoIterator<Item = T>) -> Vec<String> {
    let mut out: Vec<String> = values
        .into_iter()
        .map(|value| format!("{value:?}"))
        .collect();
    out.sort();
    out
}

fn tree(roots: &[&Path]) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let mut stack: Vec<PathBuf> = roots.iter().map(|root| root.to_path_buf()).collect();
    while let Some(path) = stack.pop() {
        out.insert(path.clone());
        if path.is_dir() {
            for entry in fs::read_dir(&path).expect("dir") {
                stack.push(entry.expect("entry").path());
            }
        }
    }
    out
}

fn remove_db(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

fn nz(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("nonzero")
}

fn summary_request(workspace: WorkspaceId) -> StructuralSummaryRequest {
    StructuralSummaryRequest {
        workspace,
        grouping: GroupingSpec::DirectoryDepth {
            root: String::new(),
            depth: 1,
        },
        resource_scope: ResourceScope::default(),
        relation_kinds: Vec::new(),
        include_ungrouped: true,
        include_cycles: true,
        member_sample_limit: None,
    }
}

/// Query surface code, comments and tests excluded.
fn surface_code() -> String {
    include_str!("../../../query_surface.rs")
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ------------------------------------------------------- binding / global

#[test]
fn binding_reports_not_initialized_and_never_creates_a_database() {
    // 1
    let fixture = Fixture::standard("surface-binding");
    let before = tree(&[&fixture._home.0, &fixture._root.0]);
    assert!(matches!(
        CoreQuerySurface::open(&fixture.global.global_db, WorkspaceId::generate()),
        Err(CoreError::NotInitialized(
            NotInitialized::WorkspaceNotRegistered
        ))
    ));
    assert!(matches!(
        CoreQuerySurface::open(&fixture.root.join("no-global.db"), fixture.workspace),
        Err(CoreError::NotInitialized(NotInitialized::GlobalDbMissing))
    ));
    assert_eq!(tree(&[&fixture._home.0, &fixture._root.0]), before);

    remove_db(&fixture.paths.workspace_db);
    assert!(matches!(
        CoreQuerySurface::open(&fixture.global.global_db, fixture.workspace),
        Err(CoreError::NotInitialized(
            NotInitialized::WorkspaceDbMissing
        ))
    ));
    assert!(!fixture.paths.workspace_db.exists(), "not created");

    let fixture = Fixture::standard("surface-binding-index");
    remove_db(&fixture.paths.index_db);
    let before = tree(&[&fixture._home.0, &fixture._root.0]);
    assert!(matches!(
        CoreQuerySurface::open(&fixture.global.global_db, fixture.workspace),
        Err(CoreError::NotInitialized(NotInitialized::IndexDbMissing))
    ));
    assert_eq!(tree(&[&fixture._home.0, &fixture._root.0]), before);
}

#[test]
fn resolve_workspace_returns_one_or_says_why_not() {
    // 2
    let fixture = Fixture::standard("surface-resolve");
    let global = &fixture.global.global_db;
    assert_eq!(
        CoreQuerySurface::resolve_workspace(global, &fixture.root).expect("one"),
        fixture.workspace
    );
    assert!(matches!(
        CoreQuerySurface::resolve_workspace(global, &fixture.root.join("src")),
        Err(CoreError::NotInitialized(
            NotInitialized::WorkspaceNotRegistered
        ))
    ));
    assert!(matches!(
        CoreQuerySurface::resolve_workspace(&fixture.root.join("none.db"), &fixture.root),
        Err(CoreError::NotInitialized(NotInitialized::GlobalDbMissing))
    ));

    let registry = GlobalRegistry::open(global).expect("registry");
    let project = registry
        .get_workspace(fixture.workspace)
        .expect("lookup")
        .expect("registered")
        .project_id;
    let second = WorkspaceId::generate();
    registry
        .register_workspace(second, project, &fixture.root, false)
        .expect("second Workspace at the same locator");
    match CoreQuerySurface::resolve_workspace(global, &fixture.root) {
        Err(CoreError::WorkspaceLocatorAmbiguous { workspaces }) => {
            assert_eq!(
                workspaces.into_iter().collect::<BTreeSet<_>>(),
                BTreeSet::from([fixture.workspace, second]),
                "none picked"
            );
        }
        other => panic!("ambiguous: {other:?}"),
    }
}

fn run_git(args: &[&str], cwd: &Path) {
    let output = process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "brainprint-test")
        .env("GIT_AUTHOR_EMAIL", "test@brainprint.invalid")
        .env("GIT_COMMITTER_NAME", "brainprint-test")
        .env("GIT_COMMITTER_EMAIL", "test@brainprint.invalid")
        .output()
        .expect("git");
    assert!(output.status.success(), "git {args:?}");
}

/// Two worktrees of one Project, each an initialized, published Workspace.
fn worktrees() -> (Fixture, Fixture) {
    let home = TestDir::create("surface-wt-home");
    let main = TestDir::create("surface-wt-main");
    let secondary = TestDir::create("surface-wt-secondary");
    fs::remove_dir_all(&secondary.0).expect("the worktree target must not exist");
    fs::create_dir_all(main.0.join("src")).expect("dirs");
    fs::write(main.0.join("src/shared.ts"), SHARED_TS).expect("file");
    run_git(&["init", "-q"], &main.0);
    run_git(&["add", "."], &main.0);
    run_git(&["commit", "-q", "-m", "init"], &main.0);
    run_git(
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            &secondary.0.to_string_lossy(),
        ],
        &main.0,
    );
    let home_path = home.0.clone();
    let a = Fixture::init(home, main);
    // B under the same home, so one registry holds both.
    let global = GlobalPaths::from_home(&home_path);
    let outcome = init_workspace(&secondary.0, &global).expect("secondary init");
    let paths = WorkspacePaths::from_root(&outcome.workspace_root);
    BaselineScan::open(&paths.index_db)
        .expect("index.db")
        .run_initial_scan(
            &outcome.workspace_root,
            &WorkspaceConfig::default(),
            "workspace-rev-1",
        )
        .expect("baseline scan");
    let b = Fixture {
        _home: TestDir::create("surface-wt-unused"),
        _root: secondary,
        global,
        workspace: outcome.workspace_id,
        root: outcome.workspace_root,
        paths,
    };
    a.publish(&[]);
    b.publish(&[]);
    (a, b)
}

#[test]
fn a_handle_answers_only_for_its_own_workspace() {
    // 3, 42
    let (a, b) = worktrees();
    let surface_a = surface(&a);
    let registry = GlobalRegistry::open(&a.global.global_db).expect("registry");
    let project = |ws| {
        registry
            .get_workspace(ws)
            .expect("lookup")
            .expect("registered")
            .project_id
    };
    assert_eq!(project(a.workspace), project(b.workspace), "one Project");
    assert_ne!(a.workspace, b.workspace);

    let theirs = b.work_item("feature work", &[]);
    let mine = a.work_item("main work", &[]);

    // Another Workspace's request is refused outright.
    let foreign = ctx(&b);
    assert!(matches!(
        surface_a.relations(RelationsRequest {
            context: foreign.clone(),
            target: named(SymbolName::Name("shared".to_owned())),
            direction: RelationDirection::Both,
            kinds: Vec::new(),
        }),
        Err(CoreError::WorkspaceMismatch { .. })
    ));
    assert!(matches!(
        surface_a.knowledge(KnowledgeRequest {
            context: foreign.clone(),
            query: KnowledgeQuery::WorkItems {
                statuses: vec![WorkItemStatus::Active],
                limit: nz(10),
            },
        }),
        Err(CoreError::WorkspaceMismatch { .. })
    ));
    assert!(matches!(
        surface_a.structure(&summary_request(b.workspace)),
        Err(CoreError::WorkspaceMismatch { .. })
    ));
    assert!(matches!(
        inspect_with(
            &surface_a,
            foreign,
            named(SymbolName::Name("shared".to_owned())),
            wide(),
            &mut ledger()
        ),
        Err(CoreError::WorkspaceMismatch { .. })
    ));

    // B's WorkItem id through A's handle is simply not there.
    let resume = |work_item| {
        let mut request = ProjectionRequest::new(a.workspace, ProjectionIntent::ResumeHandoff);
        request.work_item = Some(work_item);
        surface_a.context(context_request(ctx(&a), &request, wide()), &mut ledger())
    };
    assert!(matches!(
        resume(theirs),
        Err(CoreError::WorkItemNotFound { work_item }) if work_item == theirs
    ));
    assert!(resume(mine).is_ok());
    let mut change = ProjectionRequest::new(a.workspace, ProjectionIntent::Change(None));
    change.target = Some(at(&a, "src/shared.ts", "shared"));
    change.work_item = Some(theirs);
    assert!(matches!(
        surface_a.context(context_request(ctx(&a), &change, wide()), &mut ledger()),
        Err(CoreError::WorkItemNotFound { .. })
    ));
    assert!(matches!(
        surface_a.knowledge(KnowledgeRequest {
            context: ctx(&a),
            query: KnowledgeQuery::Handoffs {
                work_item: theirs,
                limit: nz(5),
            },
        }),
        Err(CoreError::WorkItemNotFound { .. })
    ));
    let KnowledgeResult::WorkItems { items, .. } = knowledge(
        &surface_a,
        &a,
        KnowledgeQuery::WorkItems {
            statuses: vec![WorkItemStatus::Active],
            limit: nz(10),
        },
    ) else {
        panic!("work items")
    };
    assert_eq!(
        items.iter().map(|item| item.uid).collect::<Vec<_>>(),
        [mine],
        "never B's"
    );
}

#[test]
fn the_surface_cannot_reach_a_backend_or_decide_for_the_agent() {
    // 4, 31, 51
    let code = surface_code();
    for forbidden in [
        "lsp",
        "semantic_lifecycle",
        "_semantic",
        "SemanticIndex",
        "runtime::",
        "graph_lifecycle",
        "Launcher",
        "Supervisor",
        "std::process",
        "Command::new",
        "BaselineScan",
        "init_workspace",
        "TargetedRefresh",
        "bootstrap",
        "acknowledge(",
        "rusqlite",
        "execute(",
        "SELECT",
        "recommend",
        "safe_to",
        "confidence",
        "score",
        "daemon",
        "mcp",
    ] {
        assert!(!code.contains(forbidden), "{forbidden}");
    }
    // The planner it composes has no launcher path either (task 6 test).
    assert!(!planner_code().contains("Launcher"));
}

#[test]
fn the_same_truth_gives_the_same_answer() {
    // 5
    let fixture = Fixture::standard("surface-determinism");
    let request = change_shared(&fixture);
    let surface = surface(&fixture);
    let run = |surface: &CoreQuerySurface| {
        (
            inspect(surface, &fixture, at(&fixture, "src/app.ts", "run")),
            surface
                .context(
                    context_request(ctx(&fixture), &request, wide()),
                    &mut ledger(),
                )
                .expect("context"),
        )
    };
    let first = run(&surface);
    assert_eq!(first, run(&surface), "deep equality, receipts included");
    assert_eq!(
        first.0.delivery.receipt.page_fingerprint,
        run(&surface).0.delivery.receipt.page_fingerprint
    );

    let mut reversed = fixture.standard_graph();
    reversed.reverse();
    fixture.publish(&reversed);
    assert_eq!(
        first.0,
        inspect(&surface, &fixture, at(&fixture, "src/app.ts", "run")),
        "row insertion order means nothing"
    );
}

/// Every operation once on `anchor`; the call counts it caused.
fn call_counts(fixture: &Fixture, anchor: &GraphEndpoint) -> Vec<(SurfaceStats, [u64; 5])> {
    let surface = surface(fixture);
    let target = || ProjectionTarget::Endpoint(anchor.clone());
    let mut out = Vec::new();
    let mut step = |run: &dyn Fn(&CoreQuerySurface)| {
        let (stats, planner) = (surface.stats(), surface.planner_stats());
        run(&surface);
        let (after, planner_after) = (surface.stats(), surface.planner_stats());
        let delta = SurfaceStats {
            plans: after.plans - stats.plans,
            deliveries: after.deliveries - stats.deliveries,
            relation_queries: after.relation_queries - stats.relation_queries,
            file_listings: after.file_listings - stats.file_listings,
            text_searches: after.text_searches - stats.text_searches,
            rule_resolves: after.rule_resolves - stats.rule_resolves,
            work_item_lists: after.work_item_lists - stats.work_item_lists,
            lineage_reads: after.lineage_reads - stats.lineage_reads,
            handoff_lists: after.handoff_lists - stats.handoff_lists,
            summaries: after.summaries - stats.summaries,
        };
        out.push((
            delta,
            [
                planner_after.plans - planner.plans,
                planner_after.relation_queries - planner.relation_queries,
                planner_after.impact_traversals - planner.impact_traversals,
                planner_after.knowledge_resolves - planner.knowledge_resolves,
                planner_after.work_snapshots - planner.work_snapshots,
            ],
        ));
    };
    step(&|surface| {
        find_target(surface, fixture, target());
    });
    step(&|surface| {
        inspect(surface, fixture, target());
    });
    step(&|surface| {
        relations_of(
            surface,
            fixture,
            target(),
            RelationDirection::Both,
            Vec::new(),
        );
    });
    step(&|surface| {
        impact(
            surface,
            fixture,
            target(),
            ChangeKind::Structural(ImpactIntent::Rename),
        );
    });
    step(&|surface| {
        let mut request = fixture.request(ProjectionIntent::Change(None), Some(target()));
        request.knowledge.decision_topics.insert("orm".to_owned());
        surface
            .context(
                context_request(ctx(fixture), &request, wide()),
                &mut ledger(),
            )
            .expect("context");
    });
    step(&|surface| {
        knowledge(
            surface,
            fixture,
            KnowledgeQuery::Rules {
                scope_layers: Vec::new(),
                directives: Vec::new(),
                knowledge: ProjectionKnowledgeRefs::default(),
            },
        );
    });
    step(&|surface| {
        surface
            .structure(&summary_request(fixture.workspace))
            .expect("structure");
    });
    out
}

#[test]
fn call_counts_are_fixed_per_operation() {
    // 6
    let standard = Fixture::standard("surface-counts");
    let small = call_counts(&standard, &standard.endpoint("src/shared.ts", "shared"));
    let (broad, hub) = broad_fixture("surface-counts-broad");
    let large = call_counts(&broad, &hub);
    assert_eq!(small, large, "40 callers do not add calls");
    let facade = |plans, deliveries, relation_queries, rule_resolves, summaries| SurfaceStats {
        plans,
        deliveries,
        relation_queries,
        rule_resolves,
        summaries,
        ..SurfaceStats::default()
    };
    assert_eq!(
        small,
        vec![
            (facade(1, 1, 0, 0, 0), [1, 0, 0, 0, 0]), // find
            (facade(1, 1, 0, 0, 0), [1, 2, 0, 0, 0]), // inspect
            (facade(1, 0, 2, 0, 0), [1, 0, 0, 0, 0]), // relations
            (facade(1, 1, 0, 0, 0), [1, 0, 1, 0, 0]), // impact
            (facade(1, 1, 0, 0, 0), [1, 2, 0, 1, 0]), // context
            (facade(0, 0, 0, 1, 0), [0, 0, 0, 1, 0]), // knowledge
            (facade(0, 0, 0, 0, 1), [0, 0, 0, 0, 0]), // structure
        ]
    );
}

// ------------------------------------------------------------------- find

#[test]
fn exact_selectors_resolve_to_their_identity() {
    // 7
    let fixture = Fixture::standard("surface-exact");
    let surface = surface(&fixture);
    let shared = fixture.endpoint("src/shared.ts", "shared");
    let file = fixture.file("src/shared.ts");
    let crate::graph::GraphEndpoint::Resource(file_id) = file.clone() else {
        unreachable!()
    };
    for (target, expected) in [
        (ProjectionTarget::Endpoint(file.clone()), file.clone()),
        (ProjectionTarget::Endpoint(shared.clone()), shared.clone()),
        (
            ProjectionTarget::Resource(ResourceTarget::Id(file_id)),
            file.clone(),
        ),
        (
            ProjectionTarget::Resource(ResourceTarget::Path("src/shared.ts".to_owned())),
            file.clone(),
        ),
        (
            named(SymbolName::QualifiedName("shared".to_owned())),
            shared.clone(),
        ),
    ] {
        assert_eq!(
            find_target(&surface, &fixture, target).target,
            TargetResolution::Resolved(expected)
        );
    }
}

#[test]
fn several_candidates_are_returned_never_picked() {
    // 8
    let fixture = Fixture::standard("surface-ambiguous");
    let surface = surface(&fixture);
    let answer = find_target(
        &surface,
        &fixture,
        named(SymbolName::Name("run".to_owned())),
    );
    assert_eq!(answer.target, TargetResolution::MultipleCandidates);
    let selection = selection_of(page(&answer));
    assert_eq!(selection.located.candidates.len(), 2);
    assert_eq!(selection.located.exact(), None);
    assert!(!selection.located.truncated);
    let again = find_target(
        &surface,
        &fixture,
        named(SymbolName::Name("run".to_owned())),
    );
    assert_eq!(
        selection_of(page(&again)).located.candidates,
        selection.located.candidates,
        "deterministic order"
    );

    let texts: Vec<(String, String)> = (0..=DEFAULT_CANDIDATE_LIMIT)
        .map(|index| {
            (
                format!("src/d{index:03}.ts"),
                "export function dup(): number {\n  return 0\n}\n".to_owned(),
            )
        })
        .collect();
    let files: Vec<(&str, &str)> = texts
        .iter()
        .map(|(rel, text)| (rel.as_str(), text.as_str()))
        .collect();
    let many = Fixture::with_files("surface-truncated", &files);
    let surface = self::surface(&many);
    let answer = find_target(&surface, &many, named(SymbolName::Name("dup".to_owned())));
    assert_eq!(answer.target, TargetResolution::MultipleCandidates);
    let selection = selection_of(page(&answer));
    assert!(selection.located.truncated);
    assert_eq!(selection.located.candidates.len(), DEFAULT_CANDIDATE_LIMIT);
}

#[test]
fn one_search_hit_is_not_an_identity() {
    // 9
    let fixture = Fixture::standard("surface-not-exact");
    let answer = find_target(
        &surface(&fixture),
        &fixture,
        named(SymbolName::PartialName("unique_fragment".to_owned())),
    );
    assert_eq!(answer.target, TargetResolution::SingleNonExactCandidate);
    assert_eq!(selection_of(page(&answer)).located.candidates.len(), 1);
}

#[test]
fn not_found_says_whether_coverage_allows_none() {
    // 10
    let fixture = Fixture::standard("surface-not-found");
    let answer = find_target(
        &surface(&fixture),
        &fixture,
        ProjectionTarget::Resource(ResourceTarget::Path("src/missing.ts".to_owned())),
    );
    assert_eq!(answer.target, TargetResolution::NotFound);

    let partial = Fixture::with_files(
        "surface-incomplete",
        &[
            ("src/a.ts", UNIQUE_TS),
            (
                "ui/Widget.svelte",
                "<script>\n  export function mount() {}\n</script>\n<p>hi</p>\n",
            ),
        ],
    );
    let answer = find_target(
        &surface(&partial),
        &partial,
        named(SymbolName::Name("never_declared".to_owned())),
    );
    assert_eq!(answer.target, TargetResolution::NotFoundIncompleteCoverage);
    assert!(has(&answer, |item| matches!(
        item,
        EvidenceItem::Coverage(coverage)
            if matches!(coverage.subject, CoverageSubject::TargetSelection(_))
                && coverage.answer_state() == AnswerState::NoneWithIncompleteCoverage
    )));
}

#[test]
fn an_exact_identity_that_is_not_current_is_not_current() {
    // 11
    let fixture = Fixture::standard("surface-not-current");
    let answer = find_target(
        &surface(&fixture),
        &fixture,
        ProjectionTarget::Endpoint(GraphEndpoint::Symbol(SymbolId::generate())),
    );
    assert_eq!(answer.target, TargetResolution::NotCurrent);
}

#[test]
fn target_and_files_modes_read_no_source() {
    // 12, 13
    let fixture = Fixture::standard("surface-files");
    let surface = surface(&fixture);
    let answer = find_target(
        &surface,
        &fixture,
        named(SymbolName::Name("shared".to_owned())),
    );
    assert!(!any_source(&answer));

    let files = |limit| {
        surface.find(
            FindRequest {
                context: ctx(&fixture),
                query: FindQuery::Files {
                    directory: None,
                    recursive: false,
                    path_prefix: None,
                    role: None,
                    language: None,
                    kind: None,
                    limit: nz(limit),
                },
            },
            &mut ledger(),
        )
    };
    let all = QueryIndex::open(&fixture.paths.index_db)
        .expect("index")
        .list_files(&crate::query::FileQuery::default())
        .expect("files");
    let Ok(FindResult::Files(two)) = files(2) else {
        panic!("files")
    };
    assert_eq!(two.entries.len(), 2);
    assert!(two.truncated);
    assert_eq!(two.entries[..], all.entries[..2]);
    let Ok(FindResult::Files(every)) = files(DEFAULT_CANDIDATE_LIMIT) else {
        panic!("files")
    };
    assert!(!every.truncated);
    assert_eq!(every, all);
    assert_eq!(
        every.currentness,
        fixture
            .planner()
            .index()
            .currentness()
            .expect("currentness")
    );
    assert!(matches!(
        files(DEFAULT_CANDIDATE_LIMIT + 1),
        Err(CoreError::InvalidRequest(
            InvalidRequest::ListLimitTooLarge {
                limit: 201,
                max: 200
            }
        ))
    ));
    assert_eq!(surface.planner_stats().source_file_reads, 0);
    assert_eq!(surface.planner_stats().source_bytes, 0);
}

#[test]
fn text_search_is_explicit_bounded_and_honest() {
    // 14
    let fixture = Fixture::standard("surface-text");
    let surface = surface(&fixture);
    let text = |pattern: &str, budget: SearchBudget| {
        surface.find(
            FindRequest {
                context: ctx(&fixture),
                query: FindQuery::Text {
                    pattern: OwnedTextPattern::Literal(pattern.to_owned()),
                    case_insensitive: false,
                    path_prefix: None,
                    budget,
                    max_file_bytes: 1 << 20,
                    with_preview: true,
                },
            },
            &mut ledger(),
        )
    };
    let roomy = SearchBudget {
        max_results: 50,
        max_files: 100,
        max_bytes: 1 << 20,
        deadline: None,
    };
    let Ok(FindResult::Text(absent)) = text("zzz_absent_zzz", roomy) else {
        panic!("text")
    };
    assert!(absent.scope.is_complete());
    assert_eq!(absent.status, QueryStatus::NotFound);
    assert!(absent.scope.bytes_scanned > 0, "text mode reads files");
    assert_eq!(absent.structural_currentness, Currentness::Current);

    let Ok(FindResult::Text(cut)) = text(
        "zzz_absent_zzz",
        SearchBudget {
            max_files: 1,
            ..roomy
        },
    ) else {
        panic!("text")
    };
    assert!(!cut.scope.is_complete());
    assert_ne!(
        cut.status,
        QueryStatus::NotFound,
        "an incomplete scope is not none"
    );

    let Ok(FindResult::Text(one)) = text(
        "shared",
        SearchBudget {
            max_results: 1,
            ..roomy
        },
    ) else {
        panic!("text")
    };
    assert_eq!(one.matches.len(), 1);
    assert_eq!(
        one.reason,
        crate::search::FallbackReason::ExplicitTextSearch
    );

    assert!(matches!(
        text(
            "shared",
            SearchBudget {
                max_results: 0,
                ..roomy
            }
        ),
        Err(CoreError::InvalidRequest(
            InvalidRequest::SearchBudgetInvalid
        ))
    ));
    assert_eq!(
        surface.planner_stats().source_bytes,
        0,
        "never through the planner"
    );
}

#[test]
fn an_empty_selector_is_an_invalid_request() {
    // 15
    let fixture = Fixture::standard("surface-empty");
    let result = surface(&fixture).find(
        FindRequest {
            context: ctx(&fixture),
            query: FindQuery::Target {
                target: named(SymbolName::Name(String::new())),
                delivery: wide(),
            },
        },
        &mut ledger(),
    );
    assert!(matches!(
        result,
        Err(CoreError::InvalidRequest(InvalidRequest::Projection(
            ProjectionRequestError::EmptySelector
        )))
    ));
}

// ---------------------------------------------------------------- inspect

#[test]
fn inspect_delivers_the_exact_current_declaration() {
    // 16, 19
    let fixture = Fixture::standard("surface-inspect");
    let surface = surface(&fixture);
    let run = fixture.endpoint("src/app.ts", "run");
    let answer = inspect(&surface, &fixture, ProjectionTarget::Endpoint(run.clone()));
    assert_eq!(answer.target, TargetResolution::Resolved(run.clone()));
    let sources = anchor_sources(&answer);
    assert_eq!(sources.len(), 1);
    let text = fs::read_to_string(fixture.root.join("src/app.ts")).expect("file");
    assert_eq!(
        sources[0].source,
        text[sources[0].span.start_byte..sources[0].span.end_byte]
    );
    assert!(sources[0].source.contains("function run("));
    for direction in [Direction::Outgoing, Direction::Incoming] {
        assert!(has(&answer, |item| matches!(
            item,
            EvidenceItem::Coverage(CoverageEvidence {
                subject: CoverageSubject::Relations { direction: seen, anchor, .. },
                ..
            }) if *seen == direction && *anchor == run
        )));
    }

    // Relation bodies are read only when the budget selects them.
    let required = match inspect_with(
        &surface,
        ctx(&fixture),
        ProjectionTarget::Endpoint(run.clone()),
        opts(budget(None, Some(1), None), NO_REUSE),
        &mut ledger(),
    ) {
        Err(CoreError::Delivery(DeliveryError::BudgetTooSmallForRequiredEvidence {
            required_items,
            ..
        })) => required_items,
        other => panic!("one byte fits no bundle: {other:?}"),
    };
    let before = surface.planner_stats();
    let tight = inspect_with(
        &surface,
        ctx(&fixture),
        ProjectionTarget::Endpoint(run),
        opts(items(required), NO_REUSE),
        &mut ledger(),
    )
    .expect("inspect");
    let after = surface.planner_stats();
    assert_eq!(
        anchor_sources(&tight).len(),
        1,
        "required on the first page"
    );
    assert!(tight.delivery.page.more_available);
    assert_eq!(
        after.optional_source_selected,
        before.optional_source_selected
    );
    assert_eq!(after.source_file_reads - before.source_file_reads, 1);
}

#[test]
fn an_unresolved_inspect_reads_nothing() {
    // 17
    let fixture = Fixture::standard("surface-unresolved");
    let surface = surface(&fixture);
    for (target, expected) in [
        (
            named(SymbolName::Name("run".to_owned())),
            TargetResolution::MultipleCandidates,
        ),
        (
            ProjectionTarget::Resource(ResourceTarget::Path("src/missing.ts".to_owned())),
            TargetResolution::NotFound,
        ),
        (
            ProjectionTarget::Endpoint(GraphEndpoint::Symbol(SymbolId::generate())),
            TargetResolution::NotCurrent,
        ),
    ] {
        let answer = inspect(&surface, &fixture, target);
        assert_eq!(answer.target, expected);
        assert!(!any_source(&answer));
    }
    let answer = inspect(
        &surface,
        &fixture,
        named(SymbolName::Name("run".to_owned())),
    );
    assert_eq!(selection_of(page(&answer)).located.candidates.len(), 2);
    let stats = surface.planner_stats();
    assert_eq!((stats.source_bytes, stats.relation_queries), (0, 0));
}

#[test]
fn changed_or_missing_source_is_unavailable_never_stale() {
    // 18
    let fixture = Fixture::standard("surface-stale");
    fs::write(
        fixture.root.join("src/app.ts"),
        APP_TS.replace("shared() + shared()", "shared() * 2"),
    )
    .expect("edit");
    fs::remove_file(fixture.root.join("src/unique.ts")).expect("delete");
    let surface = surface(&fixture);
    for (rel, name) in [
        ("src/app.ts", "run"),
        ("src/unique.ts", "holds_unique_fragment"),
    ] {
        let resource = fixture.resource(rel).id;
        let answer = inspect(&surface, &fixture, at(&fixture, rel, name));
        assert!(
            anchor_sources(&answer).is_empty(),
            "no stale text for {rel}"
        );
        assert!(has(&answer, |item| matches!(
            item,
            EvidenceItem::SourceUnavailable { resource: seen, .. } if *seen == resource
        )));
    }
}

#[test]
fn a_target_without_local_declaration_gets_no_invented_source() {
    // 20
    let fixture = Fixture::standard("surface-no-declaration");
    let surface = surface(&fixture);
    let domain = GraphEndpoint::Domain(DomainEntity {
        kind: "ROUTE".to_owned(),
        normalized_identity: "GET /health".to_owned(),
        namespace: None,
        method: Some("GET".to_owned()),
        display_label: "GET /health".to_owned(),
    });
    for endpoint in [fixture.file("src/app.ts"), react(), domain] {
        let answer = inspect(
            &surface,
            &fixture,
            ProjectionTarget::Endpoint(endpoint.clone()),
        );
        assert_eq!(answer.target, TargetResolution::Resolved(endpoint));
        assert!(anchor_sources(&answer).is_empty());
    }
}

#[test]
fn a_bundle_that_does_not_fit_is_an_error_not_a_cut() {
    // 21
    let fixture = Fixture::standard("surface-too-small");
    let result = inspect_with(
        &surface(&fixture),
        ctx(&fixture),
        at(&fixture, "src/app.ts", "run"),
        opts(budget(None, Some(1), None), NO_REUSE),
        &mut ledger(),
    );
    assert!(matches!(
        result,
        Err(CoreError::Delivery(
            DeliveryError::BudgetTooSmallForRequiredEvidence { .. }
        ))
    ));
}

// -------------------------------------------------------------- relations

#[test]
fn relations_are_filtered_exactly_by_direction_and_kind() {
    // 22, 26
    let fixture = Fixture::standard("surface-relations");
    let surface = surface(&fixture);
    let direct = RelationIndex::open(&fixture.paths.index_db).expect("index");
    let file = fixture.file("src/shared.ts");
    let target = || ProjectionTarget::Endpoint(file.clone());

    let imports = relations_of(
        &surface,
        &fixture,
        target(),
        RelationDirection::Incoming,
        vec![RelationKind::Imports],
    );
    assert_eq!(imports.target, TargetResolution::Resolved(file.clone()));
    assert_eq!(imports.answers.len(), 1);
    assert_eq!(imports.answers[0].confirmed.len(), 3);
    assert!(
        imports.answers[0]
            .confirmed
            .iter()
            .all(|relation| relation.kind == RelationKind::Imports)
    );
    assert_eq!(
        imports.answers[0],
        direct
            .incoming(&file, &[RelationKind::Imports])
            .expect("direct")
    );
    let calls = relations_of(
        &surface,
        &fixture,
        target(),
        RelationDirection::Incoming,
        vec![RelationKind::Calls],
    );
    assert!(calls.answers[0].confirmed.is_empty());

    let run = fixture.endpoint("src/app.ts", "run");
    let both = relations_of(
        &surface,
        &fixture,
        ProjectionTarget::Endpoint(run.clone()),
        RelationDirection::Both,
        Vec::new(),
    );
    assert_eq!(
        both.answers,
        vec![
            direct.outgoing(&run, &[]).expect("direct"),
            direct.incoming(&run, &[]).expect("direct"),
        ],
        "empty kinds = every kind, outgoing then incoming"
    );
    let stats = surface.planner_stats();
    assert_eq!((stats.source_file_reads, stats.source_bytes), (0, 0));

    // Ambiguous: no RelationIndex call at all.
    let before = surface.stats().relation_queries;
    let ambiguous = relations_of(
        &surface,
        &fixture,
        named(SymbolName::Name("run".to_owned())),
        RelationDirection::Both,
        Vec::new(),
    );
    assert_eq!(ambiguous.target, TargetResolution::MultipleCandidates);
    assert!(ambiguous.answers.is_empty());
    assert_eq!(
        selection_of(&ambiguous.selection).located.candidates.len(),
        2
    );
    assert_eq!(surface.stats().relation_queries, before);
}

#[test]
fn one_canonical_relation_keeps_every_occurrence() {
    // 23
    let fixture = Fixture::standard("surface-occurrences");
    let run = fixture.endpoint("src/app.ts", "run");
    let result = relations_of(
        &surface(&fixture),
        &fixture,
        ProjectionTarget::Endpoint(run),
        RelationDirection::Outgoing,
        vec![RelationKind::Calls],
    );
    let calls = &result.answers[0].confirmed;
    assert_eq!(calls.len(), 1, "one canonical edge for two call sites");
    assert_eq!(calls[0].evidence.len(), 2);
}

#[test]
fn unresolved_sites_are_gaps_and_zero_is_not_none() {
    // 24
    let fixture = Fixture::standard("surface-gaps");
    let result = relations_of(
        &surface(&fixture),
        &fixture,
        at(&fixture, "src/dyn.ts", "dynamic"),
        RelationDirection::Outgoing,
        Vec::new(),
    );
    let answer = &result.answers[0];
    assert!(answer.confirmed.is_empty());
    assert_eq!(answer.gaps.len(), 1);
    assert!(!answer.coverage.limits().is_complete());
    assert_eq!(
        answer.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );
}

#[test]
fn a_logical_symbol_answers_as_the_relation_index_defines() {
    // 25
    let fixture = Fixture::standard("surface-logical");
    let shared = fixture.symbol("src/shared.ts", "shared");
    let store = GraphStore::open(&fixture.paths.index_db).expect("index.db");
    let generation = GenerationStore::open(&fixture.paths.index_db)
        .expect("index.db")
        .current_stable()
        .expect("stable")
        .expect("published")
        .id;
    let logical: LogicalSymbolId = logical_symbol::ensure(
        store.connection(),
        &LogicalIdentity {
            context_key: "ctx".to_owned(),
            project_key: "app.csproj".to_owned(),
            qualified_name: "shared".to_owned(),
            kind: SymbolKind::Function,
            arity: 0,
            discriminator: String::new(),
        },
        generation,
    )
    .expect("logical");
    logical_symbol::declare(store.connection(), logical, shared, "ctx", generation)
        .expect("declare");
    let endpoint = GraphEndpoint::Logical(logical);
    let result = relations_of(
        &surface(&fixture),
        &fixture,
        ProjectionTarget::Endpoint(endpoint.clone()),
        RelationDirection::Both,
        Vec::new(),
    );
    assert_eq!(result.target, TargetResolution::Resolved(endpoint.clone()));
    let direct = RelationIndex::open(&fixture.paths.index_db).expect("index");
    assert_eq!(
        result.answers,
        vec![
            direct.outgoing(&endpoint, &[]).expect("direct"),
            direct.incoming(&endpoint, &[]).expect("direct"),
        ]
    );
}

// ----------------------------------------------------------------- impact

#[test]
fn impact_is_the_i3_traversal_and_its_tests() {
    // 27, 28, 31
    let fixture = Fixture::standard("surface-impact");
    let surface = surface(&fixture);
    let shared = fixture.endpoint("src/shared.ts", "shared");
    let answer = impact(
        &surface,
        &fixture,
        ProjectionTarget::Endpoint(shared.clone()),
        ChangeKind::Structural(ImpactIntent::Rename),
    );
    assert!(!answer.delivery.page.more_available);

    let tests = RelatedTests::open(&fixture.paths.index_db).expect("index");
    let walked = tests
        .traversal()
        .run(ImpactIntent::Rename, &shared, &Budget::default())
        .expect("walk");
    let related = tests.from_impact(&walked).expect("tests");
    assert_eq!(
        sorted_debug(page(&answer).iter().filter_map(|item| match item {
            EvidenceItem::Relation(relation) => Some(relation),
            _ => None,
        })),
        sorted_debug(walked.edges.iter().map(|edge| &edge.relation))
    );
    let page_tests = sorted_debug(page(&answer).iter().filter_map(|item| match item {
        EvidenceItem::RelatedTest { candidate, .. } => Some(candidate),
        _ => None,
    }));
    assert!(!page_tests.is_empty(), "related tests present");
    assert_eq!(page_tests, sorted_debug(&related.candidates));
    for subject in [
        (|subject: &CoverageSubject| matches!(subject, CoverageSubject::Impact { .. }))
            as fn(&CoverageSubject) -> bool,
        |subject: &CoverageSubject| matches!(subject, CoverageSubject::RelatedTests { .. }),
    ] {
        assert!(has(&answer, |item| matches!(
            item,
            EvidenceItem::Coverage(coverage) if subject(&coverage.subject)
        )));
    }
    let stats = surface.planner_stats();
    assert_eq!((stats.knowledge_resolves, stats.work_snapshots), (0, 0));
}

#[test]
fn a_truncated_walk_is_visible() {
    // 29
    let fixture = Fixture::with_files("surface-chain", &[("src/chain.ts", CHAIN_TS)]);
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
    let answer = impact(
        &surface(&fixture),
        &fixture,
        at(&fixture, "src/chain.ts", "e"),
        ChangeKind::Structural(ImpactIntent::Rename),
    );
    for subject in [
        (|subject: &CoverageSubject| matches!(subject, CoverageSubject::Impact { .. }))
            as fn(&CoverageSubject) -> bool,
        |subject: &CoverageSubject| matches!(subject, CoverageSubject::RelatedTests { .. }),
    ] {
        assert!(has(&answer, |item| matches!(
            item,
            EvidenceItem::Coverage(coverage)
                if subject(&coverage.subject)
                    && coverage.report.has(CoverageLimit::TraversalTruncated)
        )));
    }
}

#[test]
fn delete_and_domain_contract_are_direct_incoming_only() {
    // 30
    let fixture = Fixture::standard("surface-delete");
    let surface = surface(&fixture);
    let shared = fixture.endpoint("src/shared.ts", "shared");
    for change in [ChangeKind::Delete, ChangeKind::DomainContractChange] {
        let answer = impact(
            &surface,
            &fixture,
            ProjectionTarget::Endpoint(shared.clone()),
            change,
        );
        assert!(
            answer
                .delivery
                .page
                .gaps
                .contains(&ProjectionGap::UnsupportedImpactProfile(change))
        );
        let relations: Vec<_> = page(&answer)
            .iter()
            .filter_map(|item| match item {
                EvidenceItem::Relation(relation) => Some(relation),
                _ => None,
            })
            .collect();
        assert!(!relations.is_empty());
        assert!(relations.iter().all(|relation| relation.target == shared));
    }
}

// ---------------------------------------------------------------- context

#[test]
fn change_context_is_source_rules_and_only_named_knowledge() {
    // 32, 33, 34
    let fixture = Fixture::standard("surface-change");
    let project = fixture.project();
    let normal = project
        .insert_policy(&policy("no default exports", "exports"))
        .expect("policy");
    let protected = project
        .insert_policy(&NewPolicy {
            protection_class: ProtectionClass::ProtectedSecurity,
            ..policy("never log secrets", "secrets")
        })
        .expect("policy");
    project
        .insert_decision(&decision("orm", "none"))
        .expect("decision");
    project
        .insert_decision(&decision("unnamed", "x"))
        .expect("decision");
    project
        .upsert_project_state(&state("phase", "beta"))
        .expect("state");
    fixture
        .global_store()
        .insert_user_preference(&preference("editor", "vim"))
        .expect("preference");
    let application = blueprint_application(&project, "layered");
    let mine = fixture.work_item("rename", &[("src/shared.ts", WorkResourceRole::Target)]);
    fixture.work_item("parallel", &[("src/shared.ts", WorkResourceRole::Target)]);

    let surface = surface(&fixture);
    let mut request = fixture.request(
        ProjectionIntent::Change(None),
        Some(at(&fixture, "src/shared.ts", "shared")),
    );
    request.knowledge.decision_topics.insert("orm".to_owned());
    request.knowledge.state_keys.insert("phase".to_owned());
    request.knowledge.blueprint_applications.insert(application);
    let answer = surface
        .context(
            context_request(ctx(&fixture), &request, wide()),
            &mut ledger(),
        )
        .expect("context");
    let sources = anchor_sources(&answer);
    assert_eq!(sources.len(), 1);
    assert!(sources[0].source.contains("function shared("));
    for uid in [normal.uid, protected.uid] {
        assert!(has(&answer, |item| matches!(
            item,
            EvidenceItem::Policy(entry) if entry.item.uid == uid
        )));
    }
    let decisions: Vec<_> = page(&answer)
        .iter()
        .filter_map(|item| match item {
            EvidenceItem::Decision(entry) => Some(entry.item.topic.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(decisions, ["orm"]);
    assert!(has(&answer, |item| matches!(
        item,
        EvidenceItem::ProjectState(_)
    )));
    assert!(has(&answer, |item| matches!(
        item,
        EvidenceItem::Blueprint(_)
    )));
    assert!(!has(&answer, |item| matches!(
        item,
        EvidenceItem::Preference(_)
    )));
    assert!(!has(&answer, |item| matches!(
        item,
        EvidenceItem::WorkItem(_)
    )));

    request.work_item = Some(mine);
    let answer = surface
        .context(
            context_request(ctx(&fixture), &request, wide()),
            &mut ledger(),
        )
        .expect("context");
    assert!(has(&answer, |item| matches!(
        item,
        EvidenceItem::WorkItem(item) if item.uid == mine
    )));
    assert!(has(&answer, |item| matches!(
        item,
        EvidenceItem::WorkOverlap { .. }
    )));
}

#[test]
fn context_pages_through_the_task_7_budget_and_continuation() {
    // 35, 36
    let fixture = Fixture::standard("surface-pages");
    let request = change_shared(&fixture);
    let surface = surface(&fixture);
    let required = match surface.context(
        context_request(
            ctx(&fixture),
            &request,
            opts(budget(None, Some(1), None), NO_REUSE),
        ),
        &mut ledger(),
    ) {
        Err(CoreError::Delivery(DeliveryError::BudgetTooSmallForRequiredEvidence {
            required_items,
            ..
        })) => required_items,
        other => panic!("one byte fits no bundle: {other:?}"),
    };
    let small = items(required + 1);
    let first = surface
        .context(
            context_request(ctx(&fixture), &request, opts(small, NO_REUSE)),
            &mut ledger(),
        )
        .expect("first page");
    assert!(first.delivery.page.used_items <= required + 1);
    assert_eq!(anchor_sources(&first).len(), 1, "required on page one");
    assert!(first.delivery.page.more_available);
    let continuation = first.delivery.page.continuation.clone().expect("keyset");

    let mut next = opts(small, NO_REUSE);
    next.continuation = Some(continuation.clone());
    let second = surface
        .context(
            context_request(ctx(&fixture), &request, next.clone()),
            &mut ledger(),
        )
        .expect("second page");
    assert!(!second.delivery.page.evidence.is_empty());
    assert!(
        second
            .delivery
            .page
            .evidence
            .iter()
            .all(|item| !page(&first).contains(item)),
        "nothing repeated"
    );
    assert!(
        second.delivery.page.gaps.is_empty(),
        "gaps on page one only"
    );
    assert_eq!(second.target, first.target, "outcome holds on every page");

    // A Policy changes: the re-planned truth no longer matches.
    fixture
        .project()
        .insert_policy(&policy("a new rule", "new"))
        .expect("policy");
    assert!(matches!(
        surface.context(
            context_request(ctx(&fixture), &request, next),
            &mut ledger()
        ),
        Err(CoreError::Delivery(DeliveryError::ContinuationMismatch(
            ContinuationMismatch::Projection
        )))
    ));

    // A rebuilt index: another incarnation.
    let impact_request = |continuation: Option<DeliveryContinuation>| ImpactRequest {
        context: ctx(&fixture),
        target: at(&fixture, "src/shared.ts", "shared"),
        change: ChangeKind::Structural(ImpactIntent::Rename),
        delivery: DeliveryOptions {
            continuation,
            ..opts(items(3), NO_REUSE)
        },
    };
    let continuation = surface
        .impact(impact_request(None), &mut ledger())
        .expect("first")
        .delivery
        .page
        .continuation
        .expect("more remains");
    drop(surface);
    remove_db(&fixture.paths.index_db);
    init_workspace(&fixture.root, &fixture.global).expect("re-init");
    BaselineScan::open(&fixture.paths.index_db)
        .expect("index.db")
        .run_initial_scan(
            &fixture.root,
            &WorkspaceConfig::default(),
            "workspace-rev-1",
        )
        .expect("scan");
    fixture.publish(&fixture.standard_graph());
    let surface = self::surface(&fixture);
    assert!(matches!(
        surface.impact(impact_request(Some(continuation)), &mut ledger()),
        Err(CoreError::Delivery(DeliveryError::ContinuationMismatch(
            ContinuationMismatch::IndexIncarnation
        )))
    ));
}

#[test]
fn reuse_needs_an_acknowledgement_and_follows_retention() {
    // 37, 39
    let fixture = Fixture::standard("surface-reuse");
    let request = change_shared(&fixture);
    let surface = surface(&fixture);
    let mut ledger = ledger();
    let call = |ledger: &mut DeliveryLedger, retention| {
        surface
            .context(
                context_request(
                    session(&fixture, "s1"),
                    &request,
                    opts(items(10_000), retention),
                ),
                ledger,
            )
            .expect("context")
    };
    let first = call(&mut ledger, RETAINED);
    let unacknowledged = call(&mut ledger, RETAINED);
    assert_eq!(hits(&unacknowledged), 0, "generated is not delivered");
    assert_eq!(first, unacknowledged);

    ledger.acknowledge(&unacknowledged.delivery.receipt);
    let reused = call(&mut ledger, RETAINED);
    assert!(hits(&reused) > 0);
    assert!(reused.delivery.page.references.iter().any(Option::is_some));

    let fresh = call(&mut ledger, FRESH);
    assert_eq!(hits(&fresh), 0);
    assert!(fresh.delivery.page.references.iter().all(Option::is_none));

    let retained = call(&mut ledger, RETAINED);
    ledger.acknowledge(&retained.delivery.receipt);
    let disabled = call(&mut ledger, NO_REUSE);
    assert_eq!(disabled.delivery.receipt.scope, None, "nothing to record");
    assert_eq!(
        disabled.delivery.receipt.reuse.reusable_candidates, 0,
        "no lookup"
    );
    assert!(
        disabled
            .delivery
            .page
            .references
            .iter()
            .all(Option::is_none)
    );
}

#[test]
fn a_ledger_hit_never_skips_source_verification() {
    // 38
    let fixture = Fixture::standard("surface-verify");
    let surface = surface(&fixture);
    let mut ledger = ledger();
    let target = || named(SymbolName::QualifiedName("shared".to_owned()));
    let call = |ledger: &mut DeliveryLedger| {
        inspect_with(
            &surface,
            session(&fixture, "s1"),
            target(),
            opts(items(10_000), RETAINED),
            ledger,
        )
        .expect("inspect")
    };
    let first = call(&mut ledger);
    ledger.acknowledge(&first.delivery.receipt);
    let old = anchor_sources(&first)[0].clone();
    let reads = surface.planner_stats().source_file_reads;
    let second = call(&mut ledger);
    assert!(
        surface.planner_stats().source_file_reads > reads,
        "read again"
    );
    assert!(
        second
            .delivery
            .page
            .evidence
            .iter()
            .zip(&second.delivery.page.references)
            .any(|(item, reference)| reference.is_some()
                && matches!(item, EvidenceItem::CurrentSource(range) if *range == old))
    );

    fs::write(
        fixture.root.join("src/shared.ts"),
        SHARED_TS.replace("return 1", "return 10"),
    )
    .expect("edit");
    let changed = call(&mut ledger);
    assert!(
        !page(&changed)
            .iter()
            .any(|item| matches!(item, EvidenceItem::CurrentSource(range) if *range == old)),
        "no reference to the old version"
    );
    assert!(has(&changed, |item| matches!(
        item,
        EvidenceItem::SourceUnavailable { resource, .. } if *resource == old.resource
    )));
}

// ----------------------------------------------------------- resume / state

#[test]
fn resume_is_the_stored_state_without_repository_reads() {
    // 40, 41
    let fixture = Fixture::standard("surface-resume");
    let mine = fixture.work_item("resume me", &[("src/app.ts", WorkResourceRole::Target)]);
    let work = fixture.work();
    work.add_handoff(&WorkHandoff {
        work_item: mine,
        handoff_summary: "half done".to_owned(),
        remaining_summary: None,
        blocker_summary: None,
        next_scope_hint: None,
        created_at: String::new(),
    })
    .expect("handoff");
    work.record_partial(
        mine,
        &ResultObservation {
            summary: "committed part".to_owned(),
            commit_id: Some("abc123".to_owned()),
            change_set_fingerprint: None,
            verification_summary: None,
            remaining_dirty: DirtyObservation::Unknown,
        },
    )
    .expect("partial");
    let surface = surface(&fixture);
    let mut request = fixture.request(ProjectionIntent::ResumeHandoff, None);
    request.work_item = Some(mine);
    let answer = surface
        .context(
            context_request(ctx(&fixture), &request, wide()),
            &mut ledger(),
        )
        .expect("resume");
    assert_eq!(answer.target, TargetResolution::NoTarget);
    for pick in [
        (|item: &EvidenceItem| matches!(item, EvidenceItem::WorkingState(_)))
            as fn(&EvidenceItem) -> bool,
        |item| matches!(item, EvidenceItem::Handoff(_)),
        |item| matches!(item, EvidenceItem::GenerationReference { .. }),
        |item| matches!(item, EvidenceItem::WorkResult(result) if result.commit_id.as_deref() == Some("abc123")),
        |item| matches!(item, EvidenceItem::WorkItem(item) if item.status == WorkItemStatus::Active),
    ] {
        assert!(has(&answer, pick));
    }
    let stats = surface.planner_stats();
    assert_eq!((stats.source_file_reads, stats.source_bytes), (0, 0));
}

#[test]
fn equivalent_status_sets_give_the_same_canonical_listing() {
    // #23 fix: request order/duplicates must never change the result.
    let fixture = Fixture::standard("surface-status-canonical");
    let active = fixture.work_item("active one", &[]);
    let completed_item = fixture.work_item("completed one", &[]);
    fixture
        .work()
        .complete(
            completed_item,
            &ResultObservation {
                summary: "done".to_owned(),
                commit_id: None,
                change_set_fingerprint: None,
                verification_summary: None,
                remaining_dirty: DirtyObservation::Unknown,
            },
            None,
        )
        .expect("complete");
    let surface = surface(&fixture);
    let list = |statuses: Vec<WorkItemStatus>| {
        knowledge(
            &surface,
            &fixture,
            KnowledgeQuery::WorkItems {
                statuses,
                limit: nz(10),
            },
        )
    };
    let forward = list(vec![WorkItemStatus::Active, WorkItemStatus::Completed]);
    let reversed = list(vec![WorkItemStatus::Completed, WorkItemStatus::Active]);
    let duplicated = list(vec![
        WorkItemStatus::Active,
        WorkItemStatus::Active,
        WorkItemStatus::Completed,
    ]);
    assert_eq!(forward, reversed, "caller order must not matter");
    assert_eq!(forward, duplicated, "duplicates must not matter");
    let KnowledgeResult::WorkItems { items, truncated } = &forward else {
        panic!("work items")
    };
    assert!(!truncated);
    assert_eq!(
        items.iter().map(|item| item.uid).collect::<Vec<_>>(),
        [active, completed_item],
        "canonical vocabulary order: ACTIVE before COMPLETED"
    );

    // Per-status bounded queries are unchanged: one list_work_items call
    // per distinct status, not per requested (possibly duplicated) entry.
    let before = surface.stats().work_item_lists;
    list(vec![
        WorkItemStatus::Completed,
        WorkItemStatus::Active,
        WorkItemStatus::Active,
    ]);
    assert_eq!(surface.stats().work_item_lists - before, 2);
}

#[test]
fn work_items_and_history_are_listed_bounded_and_never_chosen() {
    // 43, 44
    let fixture = Fixture::standard("surface-lists");
    let items: Vec<WorkItemId> = (0..3)
        .map(|index| fixture.work_item(&format!("item {index}"), &[]))
        .collect();
    let surface = surface(&fixture);
    let list = |statuses: Vec<WorkItemStatus>, limit| {
        knowledge(
            &surface,
            &fixture,
            KnowledgeQuery::WorkItems {
                statuses,
                limit: nz(limit),
            },
        )
    };
    let KnowledgeResult::WorkItems {
        items: two,
        truncated,
    } = list(vec![WorkItemStatus::Active], 2)
    else {
        panic!("work items")
    };
    assert!(truncated);
    assert_eq!(
        two.iter().map(|item| item.uid).collect::<Vec<_>>(),
        items[..2]
    );
    let KnowledgeResult::WorkItems {
        items: all,
        truncated,
    } = list(
        vec![
            WorkItemStatus::Active,
            WorkItemStatus::Active,
            WorkItemStatus::Completed,
        ],
        3,
    )
    else {
        panic!("work items")
    };
    assert!(!truncated);
    assert_eq!(all.iter().map(|item| item.uid).collect::<Vec<_>>(), items);
    assert!(matches!(
        surface.knowledge(KnowledgeRequest {
            context: ctx(&fixture),
            query: KnowledgeQuery::WorkItems {
                statuses: vec![WorkItemStatus::Active],
                limit: nz(201),
            },
        }),
        Err(CoreError::InvalidRequest(
            InvalidRequest::ListLimitTooLarge { .. }
        ))
    ));

    let work = fixture.work();
    for summary in ["first", "second", "third"] {
        work.add_handoff(&WorkHandoff {
            work_item: items[0],
            handoff_summary: summary.to_owned(),
            remaining_summary: None,
            blocker_summary: None,
            next_scope_hint: None,
            created_at: String::new(),
        })
        .expect("handoff");
    }
    let KnowledgeResult::Handoffs {
        handoffs,
        truncated,
        ..
    } = knowledge(
        &surface,
        &fixture,
        KnowledgeQuery::Handoffs {
            work_item: items[0],
            limit: nz(2),
        },
    )
    else {
        panic!("handoffs")
    };
    assert!(truncated);
    assert_eq!(
        handoffs
            .iter()
            .map(|h| h.handoff_summary.as_str())
            .collect::<Vec<_>>(),
        ["third", "second"],
        "newest first"
    );
    assert!(matches!(
        surface.knowledge(KnowledgeRequest {
            context: ctx(&fixture),
            query: KnowledgeQuery::Handoffs {
                work_item: items[0],
                limit: nz(51),
            },
        }),
        Err(CoreError::InvalidRequest(
            InvalidRequest::ListLimitTooLarge { .. }
        ))
    ));

    // Lineage: one hop only.
    let project = fixture.project();
    let [p1, p2, p3]: [PolicyId; 3] = ["v1", "v2", "v3"].map(|key| {
        project
            .insert_policy(&policy(key, key))
            .expect("policy")
            .uid
    });
    project.supersede_policy(p2, p1).expect("supersede");
    project.supersede_policy(p3, p2).expect("supersede");
    let KnowledgeResult::PolicyLineage(lineage) = knowledge(
        &surface,
        &fixture,
        KnowledgeQuery::Lineage(LineageTarget::ProjectPolicy(p1)),
    ) else {
        panic!("lineage")
    };
    assert_eq!(lineage.superseded_by, [p2], "not p3");
    assert_eq!(lineage, project.policy_lineage(p1).expect("direct"));
}

#[test]
fn rules_are_the_planners_knowledge_evidence() {
    // 45
    let fixture = Fixture::standard("surface-rules");
    let project = fixture.project();
    project
        .insert_policy(&policy("applicable rule", "rule"))
        .expect("policy");
    project
        .insert_policy(&NewPolicy {
            protection_class: ProtectionClass::ProtectedSecurity,
            ..policy("never log secrets", "secrets")
        })
        .expect("policy");
    project
        .insert_decision(&decision("orm", "none"))
        .expect("decision");
    project
        .insert_decision(&decision("other", "x"))
        .expect("decision");
    project
        .upsert_project_state(&state("phase", "beta"))
        .expect("state");
    let application = blueprint_application(&project, "layered");
    let missing = BlueprintApplicationId::generate();

    let directives = vec![RequestDirective {
        id: "d1".to_owned(),
        target: DirectiveTarget::Policy,
        subject_key: "secrets".to_owned(),
        scope: KnowledgeScope::project(),
        summary: "log everything".to_owned(),
    }];
    let refs = ProjectionKnowledgeRefs {
        decision_topics: BTreeSet::from(["orm".to_owned()]),
        preference_keys: BTreeSet::new(),
        state_keys: BTreeSet::from(["phase".to_owned()]),
        blueprint_applications: BTreeSet::from([application, missing]),
    };
    let surface = surface(&fixture);
    let KnowledgeResult::Rules { evidence, gaps } = knowledge(
        &surface,
        &fixture,
        KnowledgeQuery::Rules {
            scope_layers: Vec::new(),
            directives: directives.clone(),
            knowledge: refs.clone(),
        },
    ) else {
        panic!("rules")
    };

    let mut request = fixture.request(
        ProjectionIntent::Change(None),
        Some(at(&fixture, "src/app.ts", "run")),
    );
    request.directives = directives;
    request.knowledge = refs;
    let planned = fixture.planner().plan(&request).expect("plan");
    let expected: Vec<EvidenceItem> = planned
        .evidence
        .into_iter()
        .filter(|item| {
            matches!(
                item,
                EvidenceItem::Policy(_)
                    | EvidenceItem::Directive(_)
                    | EvidenceItem::Decision(_)
                    | EvidenceItem::Preference(_)
                    | EvidenceItem::ProjectState(_)
                    | EvidenceItem::Blueprint(_)
                    | EvidenceItem::KnowledgeConflict(_)
            )
        })
        .collect();
    assert!(
        expected
            .iter()
            .any(|item| matches!(item, EvidenceItem::KnowledgeConflict(_)))
    );
    assert_eq!(evidence, expected, "same items, same order");
    assert_eq!(
        gaps,
        [ProjectionGap::BlueprintApplicationNotApplied(missing)]
    );
    assert!(planned.gaps.contains(&gaps[0]));
    let stats = surface.planner_stats();
    assert_eq!((stats.source_file_reads, stats.work_snapshots), (0, 0));
}

// -------------------------------------------------------------- structure

#[test]
fn structure_is_the_task_9_summary_and_nothing_else_touches_it() {
    // 46, 47, 48
    let fixture = Fixture::standard("surface-structure");
    let surface = surface(&fixture);
    let target = || at(&fixture, "src/shared.ts", "shared");
    find_target(&surface, &fixture, target());
    inspect(&surface, &fixture, target());
    relations_of(
        &surface,
        &fixture,
        target(),
        RelationDirection::Both,
        Vec::new(),
    );
    impact(
        &surface,
        &fixture,
        target(),
        ChangeKind::Structural(ImpactIntent::Rename),
    );
    let mut request = fixture.request(ProjectionIntent::Change(None), Some(target()));
    request.work_item = Some(fixture.work_item("w", &[]));
    surface
        .context(
            context_request(ctx(&fixture), &request, wide()),
            &mut ledger(),
        )
        .expect("context");
    knowledge(
        &surface,
        &fixture,
        KnowledgeQuery::Rules {
            scope_layers: Vec::new(),
            directives: Vec::new(),
            knowledge: ProjectionKnowledgeRefs::default(),
        },
    );
    assert_eq!(surface.summary_stats().statements, 0, "never attached");

    let request = summary_request(fixture.workspace);
    let summary = surface.structure(&request).expect("structure");
    let direct = StructuralSummaryIndex::open(&fixture.paths.index_db)
        .expect("index")
        .summarize(&request)
        .expect("direct");
    assert_eq!(summary, direct);
    assert!(matches!(
        surface.structure(&summary_request(WorkspaceId::generate())),
        Err(CoreError::WorkspaceMismatch { .. })
    ));
}

// ------------------------------------------------------------- regression

#[test]
fn schema_versions_are_unchanged() {
    // 50
    let fixture = Fixture::standard("surface-schema");
    let _ = surface(&fixture);
    assert_eq!(
        crate::schema::global::open(&fixture.global.global_db)
            .expect("global")
            .schema_version,
        4
    );
    assert_eq!(
        crate::schema::project::open(&fixture.paths.project_db)
            .expect("project")
            .schema_version,
        4
    );
    assert_eq!(
        crate::schema::workspace::open(&fixture.paths.workspace_db)
            .expect("workspace")
            .schema_version,
        5
    );
    assert_eq!(
        crate::schema::index::open(&fixture.paths.index_db)
            .expect("index")
            .schema_version,
        10
    );
}
