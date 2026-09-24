//! #20 task 6 acceptance: binding, target gate, intent profiles,
//! knowledge/work minimality, source honesty and economy, dedupe,
//! stable order, safe negatives, and no backend wake. Every fixture is
//! an initialized Workspace with a real baseline scan and a controlled
//! structural graph; no semantic backend is installed or required.

use std::{
    collections::BTreeSet,
    env, fs,
    mem::ManuallyDrop,
    path::PathBuf,
    process,
    sync::atomic::{AtomicU64, Ordering},
};

use brainprint_core::{BlueprintApplicationId, BlueprintId, SymbolId, WorkItemId, WorkspaceId};

use super::*;
use crate::{
    config::WorkspaceConfig,
    coverage::AnswerState,
    evidence::{OccurrenceRef, RelationEvidence, replace_resource_graph},
    gaps::{IntendedRelation, UnresolvedEvidence, UnresolvedReason},
    generation,
    graph::{DomainEntity, ExternalEntity, GraphStore, Relation, RelationKind},
    impact::ImpactTraversal,
    init::init_workspace,
    knowledge::{
        BlueprintDefinition, BlueprintOwnerKind, BlueprintRef, BlueprintStatus, ConflictKind,
        DirectiveTarget, DirtyObservation, GenerationReferenceState, KnowledgeScope, NewBlueprint,
        NewBlueprintApplication, NewDecision, NewPolicy, NewUserPreference, NewWorkItem,
        PriorityClass, ProjectStateStatus, ProjectStateUpdate, ProtectionClass, Provenance,
        RequestDirective, ResourceEvidence, ResultObservation, SourceKind, StartObservation,
        TypedValue, WorkHandoff, WorkItemSourceKind, WorkProgress, WorkResourceRole,
    },
    paths::GlobalPaths,
    prepare::SourceUnavailable,
    projection::{ProjectionKnowledgeRefs, ResourceTarget, SymbolName, SymbolTarget},
    resolution::{Dispatch, EvidenceBasis},
    resource::ResourceStore,
    scan::BaselineScan,
    symbol::{OccurrenceKind, SymbolStore},
};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

const APP_TS: &str = "\
import { shared } from './shared'

export function run(obj: Thing): number {
  obj.foo()
  return shared() + shared()
}
";

const OTHER_TS: &str = "\
import { shared } from './shared'
import { useState } from 'react'

export function other(): number {
  return shared()
}
";

const SHARED_TS: &str = "\
// shared helpers

export function shared(): number {
  return 1
}
";

const UTIL_RUN_TS: &str = "\
export function run(): number {
  return 2
}
";

const UNIQUE_TS: &str = "\
export function holds_unique_fragment(): number {
  return 3
}
";

const DYN_TS: &str = "\
export function dynamic(obj: Thing): number {
  return obj.bar()
}
";

const TEST_TS: &str = "\
import { shared } from '../src/shared'

export function checksShared(): number {
  return shared()
}
";

const CHAIN_TS: &str = "\
export function a(): number {
  return b()
}

export function b(): number {
  return c()
}

export function c(): number {
  return d()
}

export function d(): number {
  return e()
}

export function e(): number {
  return 5
}
";

// ---------------------------------------------------------------- fixture

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "brainprint-planner-{label}-{}-{sequence}",
            process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("test directory");
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    _home: TestDir,
    _root: TestDir,
    global: GlobalPaths,
    workspace: WorkspaceId,
    root: PathBuf,
    paths: WorkspacePaths,
}

type FilePlan<'a> = (&'a str, Vec<RelationEvidence>, Vec<UnresolvedEvidence>);

impl Fixture {
    /// An initialized Workspace holding `files`, baseline-scanned.
    fn with_files(label: &str, files: &[(&str, &str)]) -> Self {
        let home = TestDir::create(&format!("{label}-home"));
        let root_dir = TestDir::create(&format!("{label}-root"));
        for (rel, contents) in files {
            let path = root_dir.0.join(rel);
            fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            fs::write(path, contents).expect("fixture file");
        }
        Self::init(home, root_dir)
    }

    fn init(home: TestDir, root_dir: TestDir) -> Self {
        let global = GlobalPaths::from_home(&home.0);
        let outcome = init_workspace(&root_dir.0, &global).expect("init");
        let paths = WorkspacePaths::from_root(&outcome.workspace_root);
        BaselineScan::open(&paths.index_db)
            .expect("index.db")
            .run_initial_scan(
                &outcome.workspace_root,
                &WorkspaceConfig::default(),
                "workspace-rev-1",
            )
            .expect("baseline scan");
        Self {
            _home: home,
            _root: root_dir,
            global,
            workspace: outcome.workspace_id,
            root: outcome.workspace_root,
            paths,
        }
    }

    /// The standard fixture with its graph published.
    fn standard(label: &str) -> Self {
        let fixture = Self::with_files(
            label,
            &[
                ("src/app.ts", APP_TS),
                ("src/other.ts", OTHER_TS),
                ("src/shared.ts", SHARED_TS),
                ("src/util/run.ts", UTIL_RUN_TS),
                ("src/unique.ts", UNIQUE_TS),
                ("src/dyn.ts", DYN_TS),
                ("tests/shared.test.ts", TEST_TS),
            ],
        );
        fixture.publish(&fixture.standard_graph());
        fixture
    }

    fn planner(&self) -> ProjectionPlanner {
        ProjectionPlanner::open(&self.global.global_db, self.workspace).expect("planner")
    }

    fn request(
        &self,
        intent: ProjectionIntent,
        target: Option<ProjectionTarget>,
    ) -> ProjectionRequest {
        let mut request = ProjectionRequest::new(self.workspace, intent);
        request.target = target;
        request
    }

    fn resource(&self, rel: &str) -> crate::resource::Resource {
        ResourceStore::open(&self.paths.index_db)
            .expect("index.db")
            .get_active_by_path_key(rel)
            .expect("lookup")
            .expect("the fixture file is a Resource")
    }

    fn symbol(&self, rel: &str, qualified_name: &str) -> SymbolId {
        SymbolStore::open(&self.paths.index_db)
            .expect("index.db")
            .list_for_resource(self.resource(rel).id)
            .expect("symbols")
            .into_iter()
            .find(|symbol| symbol.qualified_name == qualified_name)
            .expect("the declaration is indexed")
            .id
    }

    fn endpoint(&self, rel: &str, qualified_name: &str) -> GraphEndpoint {
        GraphEndpoint::Symbol(self.symbol(rel, qualified_name))
    }

    fn file(&self, rel: &str) -> GraphEndpoint {
        GraphEndpoint::Resource(self.resource(rel).id)
    }

    fn sites(&self, rel: &str, kind: OccurrenceKind) -> Vec<OccurrenceRef> {
        SymbolStore::open(&self.paths.index_db)
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

    fn calls(&self, rel: &str) -> Vec<OccurrenceRef> {
        self.sites(rel, OccurrenceKind::CallSite)
    }

    /// Import Occurrences that are the module specifier.
    fn import_specifiers(&self, rel: &str) -> Vec<OccurrenceRef> {
        let source = fs::read_to_string(self.root.join(rel)).expect("source");
        self.sites(rel, OccurrenceKind::ImportSite)
            .into_iter()
            .filter(|site| source[site.start_byte..site.end_byte].starts_with('\''))
            .collect()
    }

    fn basis(&self, rel: &str, generation_id: i64) -> EvidenceBasis {
        let resource = self.resource(rel);
        let profile_id = SymbolStore::open(&self.paths.index_db)
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

    fn standard_graph(&self) -> Vec<FilePlan<'static>> {
        let run = self.endpoint("src/app.ts", "run");
        let other = self.endpoint("src/other.ts", "other");
        let shared = self.endpoint("src/shared.ts", "shared");
        let checks = self.endpoint("tests/shared.test.ts", "checksShared");
        let shared_file = self.file("src/shared.ts");
        let app_calls = self.calls("src/app.ts");
        let other_calls = self.calls("src/other.ts");
        let test_calls = self.calls("tests/shared.test.ts");
        vec![
            (
                "src/app.ts",
                vec![
                    evidence(app_calls[1], edge(RelationKind::Calls, &run, &shared)),
                    evidence(app_calls[2], edge(RelationKind::Calls, &run, &shared)),
                    evidence(
                        self.import_specifiers("src/app.ts")[0],
                        edge(
                            RelationKind::Imports,
                            &self.file("src/app.ts"),
                            &shared_file,
                        ),
                    ),
                ],
                vec![unresolved(app_calls[0], "foo")],
            ),
            (
                "src/other.ts",
                vec![
                    evidence(other_calls[0], edge(RelationKind::Calls, &other, &shared)),
                    evidence(
                        self.import_specifiers("src/other.ts")[0],
                        edge(
                            RelationKind::Imports,
                            &self.file("src/other.ts"),
                            &shared_file,
                        ),
                    ),
                    evidence(
                        self.import_specifiers("src/other.ts")[1],
                        edge(RelationKind::Imports, &self.file("src/other.ts"), &react()),
                    ),
                ],
                Vec::new(),
            ),
            (
                "tests/shared.test.ts",
                vec![
                    evidence(test_calls[0], edge(RelationKind::Calls, &checks, &shared)),
                    evidence(
                        self.import_specifiers("tests/shared.test.ts")[0],
                        edge(
                            RelationKind::Imports,
                            &self.file("tests/shared.test.ts"),
                            &shared_file,
                        ),
                    ),
                ],
                Vec::new(),
            ),
            (
                "src/dyn.ts",
                Vec::new(),
                vec![unresolved(self.calls("src/dyn.ts")[0], "bar")],
            ),
            ("src/shared.ts", Vec::new(), Vec::new()),
            ("src/util/run.ts", Vec::new(), Vec::new()),
            ("src/unique.ts", Vec::new(), Vec::new()),
        ]
    }

    /// Publish `plan` as one stable generation, in the order given.
    fn publish(&self, plan: &[FilePlan<'_>]) {
        let store = GraphStore::open(&self.paths.index_db).expect("index.db");
        let connection = store.connection();
        let revision = generation::current_workspace_revision(connection)
            .expect("clock")
            .expect("bootstrapped");
        let building = generation::begin_generation(connection, &revision).expect("begin");
        let transaction = connection.unchecked_transaction().expect("transaction");
        let (record, grant) =
            generation::grant_publication(&transaction, building.id).expect("grant");
        for (rel, resolved, gaps) in plan {
            let resolved: Vec<RelationEvidence> = resolved
                .iter()
                .map(|item| RelationEvidence {
                    occurrence: item.occurrence,
                    relation: Relation {
                        created_generation: building.id,
                        ..item.relation.clone()
                    },
                })
                .collect();
            for item in &resolved {
                for endpoint in [&item.relation.source, &item.relation.target] {
                    graph::ensure_entity(&transaction, endpoint).expect("ensure");
                }
            }
            replace_resource_graph(
                &transaction,
                &grant,
                &self.basis(rel, building.id),
                &resolved,
                gaps,
            )
            .expect("replace");
        }
        generation::finish_publish_stable(&transaction, &record).expect("stable");
        transaction.commit().expect("commit");
    }

    fn project(&self) -> ProjectKnowledgeStore {
        let registry = GlobalRegistry::open(&self.global.global_db).expect("registry");
        let entry = registry
            .get_workspace(self.workspace)
            .expect("lookup")
            .expect("registered");
        ProjectKnowledgeStore::open_project_home(&registry, entry.project_id).expect("project")
    }

    fn global_store(&self) -> GlobalKnowledgeStore {
        GlobalKnowledgeStore::open(&self.global.global_db).expect("global")
    }

    fn work(&self) -> WorkRuntime {
        WorkRuntime::open(
            self.workspace,
            &self.paths.workspace_db,
            &self.paths.index_db,
        )
        .expect("work runtime")
    }

    /// A started WorkItem whose edit scope names `resources`.
    fn work_item(&self, goal: &str, resources: &[(&str, WorkResourceRole)]) -> WorkItemId {
        let work = self.work();
        let item = work
            .create(&NewWorkItem {
                source_kind: WorkItemSourceKind::Issue,
                source_ref: Some("#20".to_owned()),
                title: None,
                goal: goal.to_owned(),
            })
            .expect("create");
        work.start(
            item.uid,
            &StartObservation {
                head: None,
                dirty: DirtyObservation::Unknown,
                preexisting_dirty: Vec::new(),
                owner_agent: None,
            },
        )
        .expect("start");
        let evidence: Vec<ResourceEvidence> = resources
            .iter()
            .map(|(rel, role)| ResourceEvidence {
                resource: self.resource(rel).id,
                role: *role,
                locator_hint: None,
            })
            .collect();
        work.update_progress(
            item.uid,
            &WorkProgress {
                current_step: Some(format!("{goal} step")),
                ..WorkProgress::default()
            },
            &evidence,
        )
        .expect("progress");
        item.uid
    }
}

fn edge(kind: RelationKind, source: &GraphEndpoint, target: &GraphEndpoint) -> Relation {
    Relation {
        kind,
        source: source.clone(),
        target: target.clone(),
        dispatch: Dispatch::Static,
        created_generation: 0,
    }
}

fn evidence(occurrence: OccurrenceRef, relation: Relation) -> RelationEvidence {
    RelationEvidence {
        occurrence,
        relation,
    }
}

fn unresolved(occurrence: OccurrenceRef, name: &str) -> UnresolvedEvidence {
    UnresolvedEvidence {
        occurrence,
        intended: IntendedRelation::Known(RelationKind::Calls),
        lookup_name: name.to_owned(),
        module_hint: None,
        reason: UnresolvedReason::ReceiverTypeRequired,
        candidates: Vec::new(),
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

fn symbol(name: SymbolName) -> Option<ProjectionTarget> {
    Some(ProjectionTarget::Symbol(SymbolTarget::new(name)))
}

fn provenance(source_kind: SourceKind) -> Provenance {
    Provenance::new(source_kind).with_locator("issue#20")
}

fn policy(title: &str, key: &str) -> NewPolicy {
    NewPolicy {
        scope: KnowledgeScope::project(),
        policy_key: Some(key.to_owned()),
        title: title.to_owned(),
        rule_text: format!("rule {title}"),
        structured_rule: None,
        protection_class: ProtectionClass::Normal,
        priority_class: PriorityClass::Default,
        provenance: provenance(SourceKind::UserExplicit),
    }
}

fn decision(topic: &str, chosen: &str) -> NewDecision {
    NewDecision {
        scope: KnowledgeScope::project(),
        topic: topic.to_owned(),
        chosen_summary: chosen.to_owned(),
        rationale: "because".to_owned(),
        provenance: provenance(SourceKind::UserExplicit),
    }
}

fn state(key: &str, value: &str) -> ProjectStateUpdate {
    ProjectStateUpdate {
        key: key.to_owned(),
        scope: KnowledgeScope::project(),
        value: TypedValue::Text(value.to_owned()),
        status: ProjectStateStatus::Current,
        provenance: Provenance::new(SourceKind::Observed),
    }
}

fn preference(key: &str, value: &str) -> NewUserPreference {
    NewUserPreference {
        scope: KnowledgeScope::global(),
        preference_key: key.to_owned(),
        value: TypedValue::Text(value.to_owned()),
        provenance: provenance(SourceKind::UserExplicit),
    }
}

fn blueprint_application(project: &ProjectKnowledgeStore, title: &str) -> BlueprintApplicationId {
    let blueprint: BlueprintId = project
        .insert_blueprint(&NewBlueprint {
            scope: KnowledgeScope::project(),
            blueprint_key: None,
            title: title.to_owned(),
            intent: format!("{title} intent"),
            definition: BlueprintDefinition::default(),
            status: BlueprintStatus::Active,
            version: None,
            provenance: provenance(SourceKind::UserExplicit),
        })
        .expect("blueprint")
        .uid;
    project
        .insert_blueprint_application(&NewBlueprintApplication {
            blueprint: BlueprintRef {
                owner: BlueprintOwnerKind::Project,
                uid: blueprint,
            },
            scope: KnowledgeScope::project(),
            application_summary: format!("{title} applied"),
            provenance: provenance(SourceKind::UserExplicit),
        })
        .expect("application")
        .uid
}

fn count(projection: &PreparedProjection, pick: impl Fn(&EvidenceItem) -> bool) -> usize {
    projection.evidence.iter().filter(|item| pick(item)).count()
}

fn sources(projection: &PreparedProjection) -> Vec<&PreparedRange> {
    projection
        .evidence
        .iter()
        .filter_map(|item| match item {
            EvidenceItem::CurrentSource(range) => Some(range),
            _ => None,
        })
        .collect()
}

fn relations(projection: &PreparedProjection) -> Vec<&crate::relations::RelationResult> {
    projection
        .evidence
        .iter()
        .filter_map(|item| match item {
            EvidenceItem::Relation(relation) => Some(relation),
            _ => None,
        })
        .collect()
}

fn coverage_of(
    projection: &PreparedProjection,
    pick: impl Fn(&CoverageSubject) -> bool,
) -> &CoverageEvidence {
    projection
        .evidence
        .iter()
        .find_map(|item| match item {
            EvidenceItem::Coverage(coverage) if pick(&coverage.subject) => Some(coverage),
            _ => None,
        })
        .expect("coverage for the subject")
}

fn selection(projection: &PreparedProjection) -> &TargetSelection {
    projection
        .evidence
        .iter()
        .find_map(|item| match item {
            EvidenceItem::TargetSelection(selection) => Some(selection),
            _ => None,
        })
        .expect("a target selection")
}

// ---------------------------------------------------------------- binding

#[test]
fn an_invalid_request_fails_before_any_query() {
    let fixture = Fixture::standard("invalid");
    let planner = fixture.planner();
    let request = fixture.request(ProjectionIntent::Locate, None);
    assert!(matches!(
        planner.plan(&request),
        Err(PlannerError::Request(ProjectionRequestError::MissingTarget))
    ));
    assert_eq!(planner.stats(), PlannerStats::default(), "nothing ran");

    let mut other = fixture.request(
        ProjectionIntent::Locate,
        symbol(SymbolName::Name("shared".to_owned())),
    );
    other.workspace = WorkspaceId::generate();
    assert!(matches!(
        planner.plan(&other),
        Err(PlannerError::WorkspaceMismatch { .. })
    ));
}

#[test]
fn binding_is_derived_from_the_registry_and_never_repairs() {
    let fixture = Fixture::standard("binding");
    let planner = fixture.planner();
    let registry = GlobalRegistry::open(&fixture.global.global_db).expect("registry");
    let entry = registry
        .get_workspace(fixture.workspace)
        .expect("lookup")
        .expect("registered");
    assert_eq!(planner.project_id(), entry.project_id, "Project is derived");
    drop(planner);

    assert!(matches!(
        ProjectionPlanner::open(&fixture.global.global_db, WorkspaceId::generate()),
        Err(PlannerError::UnknownWorkspace(_))
    ));
    assert!(matches!(
        ProjectionPlanner::open(&fixture.root.join("no-global.db"), fixture.workspace),
        Err(PlannerError::MissingGlobalDb)
    ));
    assert!(!fixture.root.join("no-global.db").exists(), "not created");

    // A missing index.db is reported, not created.
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", fixture.paths.index_db.display()));
    }
    assert!(matches!(
        ProjectionPlanner::open(&fixture.global.global_db, fixture.workspace),
        Err(PlannerError::Work(WorkError::MissingDatabase {
            db: "index.db"
        }))
    ));
    assert!(!fixture.paths.index_db.exists(), "index.db is not created");

    // A missing project-home project.db is reported, not created.
    let fixture = Fixture::standard("binding-project");
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", fixture.paths.project_db.display()));
    }
    assert!(matches!(
        ProjectionPlanner::open(&fixture.global.global_db, fixture.workspace),
        Err(PlannerError::Knowledge(
            KnowledgeError::ProjectHomeMissing { .. }
        ))
    ));
    assert!(!fixture.paths.project_db.exists());
}

// ------------------------------------------------------------ target gate

#[test]
fn two_same_named_symbols_stop_every_target_dependent_step() {
    let fixture = Fixture::standard("ambiguous");
    let planner = fixture.planner();
    let mut request = fixture.request(
        ProjectionIntent::Change(Some(ChangeKind::Structural(ImpactIntent::Rename))),
        symbol(SymbolName::Name("run".to_owned())),
    );
    request.knowledge.decision_topics.insert("orm".to_owned());
    let projection = planner.plan(&request).expect("plan");

    let chosen = selection(&projection);
    assert_eq!(chosen.located.candidates.len(), 2);
    assert!(chosen.located.is_ambiguous());
    assert_eq!(chosen.located.exact(), None, "no candidate chosen");
    assert_eq!(projection.target, None);
    assert!(projection.gaps.contains(&ProjectionGap::TargetAmbiguous));
    assert!(sources(&projection).is_empty(), "no source read");
    assert!(relations(&projection).is_empty(), "no graph");
    assert_eq!(
        count(&projection, |item| matches!(
            item,
            EvidenceItem::WorkOverlap { .. }
        )),
        0
    );
    let stats = planner.stats();
    assert_eq!(stats.source_file_reads, 0);
    assert_eq!(stats.impact_traversals, 0);
    assert_eq!(stats.relation_queries, 0);
}

#[test]
fn a_search_matching_once_is_not_promoted_to_an_identity() {
    let fixture = Fixture::standard("not-exact");
    let planner = fixture.planner();
    let projection = planner
        .plan(&fixture.request(
            ProjectionIntent::Understand,
            symbol(SymbolName::PartialName("unique_fragment".to_owned())),
        ))
        .expect("plan");
    let chosen = selection(&projection);
    assert_eq!(chosen.located.candidates.len(), 1);
    assert_eq!(chosen.located.exact(), None);
    assert_eq!(projection.target, None);
    assert!(projection.gaps.contains(&ProjectionGap::TargetNotExact));
    assert!(sources(&projection).is_empty());
    assert_eq!(planner.stats().relation_queries, 0);
}

#[test]
fn nothing_found_says_whether_that_means_none() {
    let fixture = Fixture::standard("not-found");
    let planner = fixture.planner();
    let projection = planner
        .plan(&fixture.request(
            ProjectionIntent::Locate,
            Some(ProjectionTarget::Resource(ResourceTarget::Path(
                "src/missing.ts".to_owned(),
            ))),
        ))
        .expect("plan");
    let coverage = coverage_of(&projection, |subject| {
        matches!(subject, CoverageSubject::TargetSelection(_))
    });
    assert_eq!(
        coverage.answer_state(),
        AnswerState::NoneUnderCompleteCoverage
    );
    assert!(projection.gaps.contains(&ProjectionGap::TargetNotFound));
}

#[test]
fn an_exact_endpoint_that_is_not_current_is_not_accepted() {
    let fixture = Fixture::standard("stale-endpoint");
    let planner = fixture.planner();
    let projection = planner
        .plan(&fixture.request(
            ProjectionIntent::Understand,
            Some(ProjectionTarget::Endpoint(GraphEndpoint::Symbol(
                SymbolId::generate(),
            ))),
        ))
        .expect("plan");
    assert_eq!(projection.target, None);
    assert!(projection.gaps.contains(&ProjectionGap::TargetNotCurrent));
    assert!(sources(&projection).is_empty());
}

// ---------------------------------------------------------------- LOCATE

#[test]
fn locate_is_identity_location_and_currentness_only() {
    let fixture = Fixture::standard("locate");
    let planner = fixture.planner();
    let projection = planner
        .plan(&fixture.request(
            ProjectionIntent::Locate,
            symbol(SymbolName::Name("shared".to_owned())),
        ))
        .expect("plan");
    let shared = fixture.endpoint("src/shared.ts", "shared");
    assert_eq!(projection.target, Some(shared));
    let EvidenceItem::Symbol(candidate) = &projection.evidence[0] else {
        panic!("identity first: {:?}", projection.evidence[0]);
    };
    assert_eq!(candidate.path_rel, "src/shared.ts");
    assert!(sources(&projection).is_empty(), "no source text");
    assert!(projection.gaps.is_empty(), "{:?}", projection.gaps);
    // A clean current LOCATE carries no redundant currentness item.
    assert_eq!(
        count(&projection, |item| matches!(
            item,
            EvidenceItem::IndexCurrentness { .. }
        )),
        0
    );
    let stats = planner.stats();
    assert_eq!(
        (
            stats.source_file_reads,
            stats.relation_queries,
            stats.impact_traversals,
            stats.knowledge_resolves,
            stats.work_snapshots,
        ),
        (0, 0, 0, 0, 0)
    );
}

// ------------------------------------------------------------ UNDERSTAND

#[test]
fn understand_reads_the_declaration_once_and_direct_edges_only() {
    let fixture = Fixture::standard("understand");
    let planner = fixture.planner();
    let run = fixture.endpoint("src/app.ts", "run");
    let projection = planner
        .plan(&fixture.request(
            ProjectionIntent::Understand,
            Some(ProjectionTarget::Endpoint(run.clone())),
        ))
        .expect("plan");

    let read = sources(&projection);
    assert_eq!(read.len(), 1, "definition source exactly once");
    assert!(read[0].source.contains("function run("));
    assert!(
        !read[0].source.contains("import"),
        "the declaration, not the file"
    );
    assert_eq!(
        read[0].resource_revision,
        fixture.resource("src/app.ts").resource_revision
    );
    assert_eq!(
        read[0].verification.expected_content_hash,
        read[0].verification.observed_content_hash
    );

    let calls: Vec<_> = relations(&projection)
        .into_iter()
        .filter(|relation| relation.source == run)
        .collect();
    assert_eq!(calls.len(), 1, "one canonical CALLS edge despite two sites");
    assert_eq!(calls[0].evidence.len(), 2);
    assert_eq!(planner.stats().impact_traversals, 0, "no transitive impact");
    assert_eq!(
        count(&projection, |item| matches!(
            item,
            EvidenceItem::RelatedTest { .. }
        )),
        0
    );
    // The unresolved receiver call is an I4 question, kept as such.
    let outgoing = coverage_of(&projection, |subject| {
        matches!(
            subject,
            CoverageSubject::Relations {
                direction: Direction::Outgoing,
                ..
            }
        )
    });
    assert!(outgoing.report.has(CoverageLimit::RequiresSemantics));
    assert!(projection.gaps.contains(&ProjectionGap::RequiresSemantics));
}

#[test]
fn changed_source_is_reported_never_returned_stale() {
    let fixture = Fixture::standard("changed");
    fs::write(
        fixture.root.join("src/app.ts"),
        APP_TS.replace("shared() + shared()", "shared() * 2"),
    )
    .expect("edit");
    let planner = fixture.planner();
    let projection = planner
        .plan(&fixture.request(
            ProjectionIntent::Understand,
            Some(ProjectionTarget::Endpoint(
                fixture.endpoint("src/app.ts", "run"),
            )),
        ))
        .expect("a changed file is a state, not a failure");
    assert!(sources(&projection).is_empty(), "no stale text");
    assert!(projection.evidence.iter().any(|item| matches!(
        item,
        EvidenceItem::SourceUnavailable {
            reason: SourceUnavailable::SourceChanged { .. },
            ..
        }
    )));
}

// ---------------------------------------------------------------- CHANGE

#[test]
fn a_structural_change_composes_knowledge_work_and_graph_minimally() {
    let fixture = Fixture::standard("change");
    let project = fixture.project();
    let rule = project
        .insert_policy(&policy("no default exports", "exports"))
        .expect("policy");
    for (topic, chosen) in [("orm", "none"), ("http", "fetch"), ("css", "tailwind")] {
        project
            .insert_decision(&decision(topic, chosen))
            .expect("decision");
    }
    for (key, value) in [("phase", "I5"), ("owner", "core"), ("freeze", "no")] {
        project
            .upsert_project_state(&state(key, value))
            .expect("state");
    }
    fixture
        .global_store()
        .insert_user_preference(&preference("indent", "2"))
        .expect("preference");
    blueprint_application(&project, "layered");

    let mine = fixture.work_item(
        "rename shared",
        &[("src/shared.ts", WorkResourceRole::Target)],
    );
    let theirs = fixture.work_item(
        "touch shared",
        &[("src/shared.ts", WorkResourceRole::Touched)],
    );
    let unrelated = fixture.work_item("docs", &[("src/unique.ts", WorkResourceRole::Target)]);

    let shared = fixture.endpoint("src/shared.ts", "shared");
    let mut request = fixture.request(
        ProjectionIntent::Change(Some(ChangeKind::Structural(ImpactIntent::Rename))),
        Some(ProjectionTarget::Endpoint(shared.clone())),
    );
    request.work_item = Some(mine);
    request.knowledge.decision_topics.insert("orm".to_owned());
    request.knowledge.state_keys.insert("phase".to_owned());
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");

    // Target source, exactly once.
    let read = sources(&projection);
    assert_eq!(read.len(), 1);
    assert!(read[0].source.contains("function shared("));

    // Applicable Policy, and only the requested subjects.
    assert!(
        projection
            .evidence
            .iter()
            .any(|item| matches!(item, EvidenceItem::Policy(entry) if entry.item.uid == rule.uid))
    );
    let decisions: Vec<_> = projection
        .evidence
        .iter()
        .filter_map(|item| match item {
            EvidenceItem::Decision(entry) => Some(entry.item.topic.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(decisions, ["orm"]);
    let states: Vec<_> = projection
        .evidence
        .iter()
        .filter_map(|item| match item {
            EvidenceItem::ProjectState(entry) => Some(entry.item.key.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(states, ["phase"]);
    assert_eq!(
        count(&projection, |item| matches!(
            item,
            EvidenceItem::Preference(_)
        )),
        0
    );
    assert_eq!(
        count(&projection, |item| matches!(
            item,
            EvidenceItem::Blueprint(_)
        )),
        0
    );

    // Impact edges and the related test from the same traversal.
    let callers: BTreeSet<String> = relations(&projection)
        .iter()
        .filter(|relation| relation.target == shared)
        .map(|relation| format!("{:?}", relation.kind))
        .collect();
    assert!(callers.contains("Calls"), "{callers:?}");
    let tests: Vec<_> = projection
        .evidence
        .iter()
        .filter_map(|item| match item {
            EvidenceItem::RelatedTest { target, candidate } if *target == shared => {
                Some(candidate.path_rel.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(tests, ["tests/shared.test.ts"]);
    coverage_of(&projection, |subject| {
        matches!(
            subject,
            CoverageSubject::Impact {
                intent: ImpactIntent::Rename,
                ..
            }
        )
    });
    coverage_of(&projection, |subject| {
        matches!(subject, CoverageSubject::RelatedTests { .. })
    });
    assert_eq!(
        planner.stats().impact_traversals,
        1,
        "one traversal, tests derived"
    );

    // Work: mine decomposed, the real overlap with theirs, nothing of the
    // unrelated one.
    assert!(
        projection
            .evidence
            .iter()
            .any(|item| matches!(item, EvidenceItem::WorkItem(item) if item.uid == mine))
    );
    assert!(
        projection.evidence.iter().any(
            |item| matches!(item, EvidenceItem::WorkingState(state) if state.work_item == mine)
        )
    );
    assert!(projection.evidence.iter().any(|item| matches!(
        item,
        EvidenceItem::WorkOverlap { work_item, overlap } if *work_item == mine && overlap.other == theirs
    )));
    for other in [theirs, unrelated] {
        assert!(!projection.evidence.iter().any(|item| matches!(
            item,
            EvidenceItem::WorkItem(item) if item.uid == other
        ) || matches!(
            item,
            EvidenceItem::WorkingState(state) if state.work_item == other
        )));
    }
    assert!(projection.evidence.iter().any(|item| matches!(
        item,
        EvidenceItem::GenerationReference {
            basis: GenerationBasis::Baseline,
            ..
        }
    )));
    assert_eq!(
        count(&projection, |item| matches!(
            item,
            EvidenceItem::WorkStaleness { .. }
        )),
        0
    );
    assert!(
        projection
            .evidence
            .iter()
            .any(|item| matches!(item, EvidenceItem::IndexCurrentness { .. }))
    );
}

#[test]
fn a_plain_edit_has_no_invented_graph_plan() {
    let fixture = Fixture::standard("plain");
    let planner = fixture.planner();
    let run = fixture.endpoint("src/app.ts", "run");
    let projection = planner
        .plan(&fixture.request(
            ProjectionIntent::Change(None),
            Some(ProjectionTarget::Endpoint(run.clone())),
        ))
        .expect("plan");
    assert_eq!(sources(&projection).len(), 1);
    assert!(
        projection
            .gaps
            .contains(&ProjectionGap::DependencyExpansionUndefined)
    );
    assert_eq!(planner.stats().impact_traversals, 0);
    assert!(
        relations(&projection)
            .iter()
            .any(|relation| relation.source == run)
    );
    assert_eq!(
        count(&projection, |item| matches!(
            item,
            EvidenceItem::RelatedTest { .. }
        )),
        0
    );
}

#[test]
fn delete_and_domain_contract_are_direct_only_and_say_so() {
    let fixture = Fixture::standard("delete");
    let planner = fixture.planner();
    let shared = fixture.endpoint("src/shared.ts", "shared");
    let projection = planner
        .plan(&fixture.request(
            ProjectionIntent::Change(Some(ChangeKind::Delete)),
            Some(ProjectionTarget::Endpoint(shared.clone())),
        ))
        .expect("plan");
    assert!(
        projection
            .gaps
            .contains(&ProjectionGap::UnsupportedImpactProfile(ChangeKind::Delete))
    );
    let found = relations(&projection);
    assert!(!found.is_empty());
    assert!(
        found
            .iter()
            .all(|relation| relation.direction == Direction::Incoming && relation.target == shared)
    );
    assert_eq!(
        planner.stats().impact_traversals,
        0,
        "not mapped to another intent"
    );
    assert_eq!(
        count(&projection, |item| matches!(
            item,
            EvidenceItem::RelatedTest { .. }
        )),
        0
    );
    assert!(
        !projection.evidence.iter().any(|item| matches!(
            item,
            EvidenceItem::Coverage(CoverageEvidence {
                subject: CoverageSubject::Impact { .. },
                ..
            })
        )),
        "no impact result is claimed"
    );

    let env = GraphEndpoint::Domain(DomainEntity {
        kind: "ENV".to_owned(),
        normalized_identity: "DATABASE_URL".to_owned(),
        namespace: None,
        method: None,
        display_label: "DATABASE_URL".to_owned(),
    });
    let projection = planner
        .plan(&fixture.request(
            ProjectionIntent::Impact(ChangeKind::DomainContractChange),
            Some(ProjectionTarget::Endpoint(env)),
        ))
        .expect("a valid request with partial capability");
    assert!(
        projection
            .gaps
            .contains(&ProjectionGap::UnsupportedImpactProfile(
                ChangeKind::DomainContractChange
            ))
    );
    assert!(relations(&projection).is_empty(), "nothing fabricated");
    let incoming = coverage_of(&projection, |subject| {
        matches!(
            subject,
            CoverageSubject::Relations {
                direction: Direction::Incoming,
                ..
            }
        )
    });
    assert_eq!(
        incoming.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );
}

// ---------------------------------------------------------------- IMPACT

#[test]
fn structural_impact_is_the_i3_traversal_unchanged() {
    let fixture = Fixture::standard("impact");
    let shared = fixture.endpoint("src/shared.ts", "shared");
    let planner = fixture.planner();
    let projection = planner
        .plan(&fixture.request(
            ProjectionIntent::Impact(ChangeKind::Structural(ImpactIntent::Rename)),
            Some(ProjectionTarget::Endpoint(shared.clone())),
        ))
        .expect("plan");
    let direct = ImpactTraversal::open(&fixture.paths.index_db)
        .expect("traversal")
        .run(ImpactIntent::Rename, &shared, &Budget::default())
        .expect("run");

    let mut expected: Vec<_> = direct.edges.iter().map(|edge| &edge.relation).collect();
    let mut planned = relations(&projection);
    let key = |relation: &&crate::relations::RelationResult| {
        (
            relation.kind.as_str(),
            graph::endpoint_sort_key(&relation.source),
            graph::endpoint_sort_key(&relation.target),
        )
    };
    expected.sort_by_key(key);
    planned.sort_by_key(key);
    assert_eq!(planned, expected, "same canonical edges");
    let coverage = coverage_of(
        &projection,
        |subject| matches!(subject, CoverageSubject::Impact { root, intent: ImpactIntent::Rename } if *root == shared),
    );
    assert_eq!(coverage.report, direct.limits());
    assert_eq!(coverage.answer_state(), direct.answer_state());
    assert!(sources(&projection).is_empty(), "impact reads no bodies");
    assert_eq!(planner.stats().source_file_reads, 0);
}

#[test]
fn zero_tests_under_a_truncated_walk_is_not_none() {
    let fixture = Fixture::with_files("chain", &[("src/chain.ts", CHAIN_TS)]);
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
    let planner = fixture.planner();
    let projection = planner
        .plan(&fixture.request(
            ProjectionIntent::Impact(ChangeKind::Structural(ImpactIntent::Rename)),
            Some(ProjectionTarget::Endpoint(
                fixture.endpoint("src/chain.ts", "e"),
            )),
        ))
        .expect("plan");
    let tests = coverage_of(&projection, |subject| {
        matches!(subject, CoverageSubject::RelatedTests { .. })
    });
    assert_eq!(tests.confirmed, 0);
    assert!(tests.report.has(CoverageLimit::TraversalTruncated));
    assert_eq!(
        tests.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );
}

#[test]
fn many_impacted_callers_do_not_become_many_reads() {
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
    let fixture = Fixture::with_files("broad", &files);
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

    let planner = fixture.planner();
    let projection = planner
        .plan(&fixture.request(
            ProjectionIntent::Change(Some(ChangeKind::Structural(ImpactIntent::Rename))),
            Some(ProjectionTarget::Endpoint(target)),
        ))
        .expect("plan");
    let impacted = relations(&projection).len();
    assert_eq!(impacted, 40);
    let stats = planner.stats();
    assert_eq!(stats.source_file_reads, 1, "the target declaration only");
    assert!(stats.source_bytes < 64, "{} bytes", stats.source_bytes);
    let optional = projection
        .source_plan
        .iter()
        .filter(|range| range.requirement == SourceRequirement::Optional)
        .count();
    assert_eq!(optional, 40, "every call site stays a locator candidate");
}

// ---------------------------------------------------------- RESUME_HANDOFF

#[test]
fn resume_projects_the_explicit_snapshot_and_nothing_else() {
    let fixture = Fixture::standard("resume");
    let mine = fixture.work_item("resume me", &[("src/app.ts", WorkResourceRole::Target)]);
    let other = fixture.work_item(
        "someone else",
        &[("src/unique.ts", WorkResourceRole::Target)],
    );
    let work = fixture.work();
    work.add_handoff(&WorkHandoff {
        work_item: mine,
        handoff_summary: "half done".to_owned(),
        remaining_summary: Some("finish run()".to_owned()),
        blocker_summary: None,
        next_scope_hint: None,
        created_at: String::new(),
    })
    .expect("handoff");
    work.record_partial(
        mine,
        &ResultObservation {
            summary: "partial".to_owned(),
            commit_id: None,
            change_set_fingerprint: None,
            verification_summary: None,
            remaining_dirty: DirtyObservation::Unknown,
        },
    )
    .expect("partial");
    fixture
        .project()
        .insert_policy(&policy("keep handoffs short", "handoff"))
        .expect("policy");

    let mut request = fixture.request(ProjectionIntent::ResumeHandoff, None);
    request.work_item = Some(mine);
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");

    for pick in [
        (|item: &EvidenceItem| matches!(item, EvidenceItem::WorkItem(_)))
            as fn(&EvidenceItem) -> bool,
        |item| matches!(item, EvidenceItem::WorkingState(_)),
        |item| matches!(item, EvidenceItem::WorkResult(_)),
        |item| matches!(item, EvidenceItem::Handoff(_)),
        |item| matches!(item, EvidenceItem::Policy(_)),
    ] {
        assert_eq!(count(&projection, pick), 1);
    }
    assert!(!projection.evidence.iter().any(|item| matches!(
        item,
        EvidenceItem::WorkItem(item) if item.uid == other
    )));
    assert!(
        sources(&projection).is_empty(),
        "no code guessed from prose"
    );
    assert_eq!(relations(&projection).len(), 0);
    let stats = planner.stats();
    assert_eq!((stats.source_file_reads, stats.relation_queries), (0, 0));
    assert_eq!(stats.work_snapshots, 1);
}

#[test]
fn a_rebuilt_index_keeps_the_generation_reference_historical() {
    let fixture = Fixture::standard("historical");
    let mine = fixture.work_item("before rebuild", &[]);
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", fixture.paths.index_db.display()));
    }
    init_workspace(&fixture.root, &fixture.global).expect("re-init binds a new index.db");

    let mut request = fixture.request(ProjectionIntent::ResumeHandoff, None);
    request.work_item = Some(mine);
    let projection = fixture.planner().plan(&request).expect("plan");
    assert!(projection.evidence.iter().any(|item| matches!(
        item,
        EvidenceItem::GenerationReference {
            basis: GenerationBasis::Baseline,
            reference,
            ..
        } if reference.state == GenerationReferenceState::HistoricalMissingOrReused
    )));
    assert!(projection.gaps.contains(&ProjectionGap::NotCurrent));
}

// -------------------------------------------------------------- knowledge

#[test]
fn only_the_named_knowledge_subjects_are_projected() {
    let fixture = Fixture::standard("knowledge");
    let project = fixture.project();
    let global = fixture.global_store();
    project
        .insert_policy(&policy("applicable rule", "rule"))
        .expect("policy");
    for index in 0..5 {
        project
            .insert_decision(&decision(&format!("topic-{index}"), "x"))
            .expect("decision");
        project
            .upsert_project_state(&state(&format!("state-{index}"), "v"))
            .expect("state");
        global
            .insert_user_preference(&preference(&format!("pref-{index}"), "p"))
            .expect("preference");
    }
    let applications: Vec<_> = (0..3)
        .map(|index| blueprint_application(&project, &format!("bp-{index}")))
        .collect();
    let mine = fixture.work_item("knowledge", &[]);

    let mut request = fixture.request(ProjectionIntent::ResumeHandoff, None);
    request.work_item = Some(mine);
    request.knowledge = ProjectionKnowledgeRefs {
        decision_topics: BTreeSet::from(["topic-2".to_owned()]),
        preference_keys: BTreeSet::from(["pref-3".to_owned()]),
        state_keys: BTreeSet::from(["state-4".to_owned()]),
        blueprint_applications: BTreeSet::from([applications[1]]),
    };
    let planner = fixture.planner();
    let projection = planner.plan(&request).expect("plan");

    let mut names = Vec::new();
    for item in &projection.evidence {
        match item {
            EvidenceItem::Policy(entry) => names.push(entry.item.title.clone()),
            EvidenceItem::Decision(entry) => names.push(entry.item.topic.clone()),
            EvidenceItem::Preference(entry) => names.push(entry.item.preference_key.clone()),
            EvidenceItem::ProjectState(entry) => names.push(entry.item.key.clone()),
            EvidenceItem::Blueprint(entry) => {
                assert_eq!(entry.item.application.uid, applications[1]);
                names.push("blueprint".to_owned());
            }
            _ => {}
        }
    }
    names.sort();
    assert_eq!(
        names,
        [
            "applicable rule",
            "blueprint",
            "pref-3",
            "state-4",
            "topic-2"
        ]
    );
    assert_eq!(planner.stats().knowledge_items, 5);

    // A named Application that does not apply is reported, not dropped.
    let missing = BlueprintApplicationId::generate();
    request.knowledge.blueprint_applications = BTreeSet::from([missing]);
    let projection = planner.plan(&request).expect("plan");
    assert!(
        projection
            .gaps
            .contains(&ProjectionGap::BlueprintApplicationNotApplied(missing))
    );
}

#[test]
fn a_protected_policy_and_its_rejected_directive_stay_task_2_facts() {
    let fixture = Fixture::standard("protected");
    let project = fixture.project();
    let protected = project
        .insert_policy(&NewPolicy {
            protection_class: ProtectionClass::ProtectedSecurity,
            ..policy("never log secrets", "secrets")
        })
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
        subject_key: "secrets".to_owned(),
        scope: KnowledgeScope::project(),
        summary: "log everything".to_owned(),
    }];
    let projection = fixture.planner().plan(&request).expect("plan");
    assert!(projection.evidence.iter().any(|item| matches!(
        item,
        EvidenceItem::Policy(entry) if entry.item.uid == protected.uid
            && entry.item.provenance.source_kind == SourceKind::UserExplicit
    )));
    assert!(projection.evidence.iter().any(|item| matches!(
        item,
        EvidenceItem::KnowledgeConflict(conflict)
            if conflict.kind == ConflictKind::ProtectedOverrideRejected
    )));
    assert_eq!(
        count(&projection, |item| matches!(
            item,
            EvidenceItem::Directive(_)
        )),
        0
    );
}

// ------------------------------------------------------ order / dedupe

fn knowledge_labels(projection: &PreparedProjection) -> Vec<String> {
    projection
        .evidence
        .iter()
        .filter_map(|item| match item {
            EvidenceItem::Policy(entry) => Some(format!("policy {}", entry.item.title)),
            EvidenceItem::Decision(entry) => Some(format!("decision {}", entry.item.topic)),
            EvidenceItem::ProjectState(entry) => Some(format!("state {}", entry.item.key)),
            _ => None,
        })
        .collect()
}

#[test]
fn row_insertion_order_does_not_change_the_projection() {
    let fixture = Fixture::standard("order");
    let shared = fixture.endpoint("src/shared.ts", "shared");
    let request = fixture.request(
        ProjectionIntent::Change(Some(ChangeKind::Structural(ImpactIntent::Rename))),
        Some(ProjectionTarget::Endpoint(shared)),
    );
    let first = fixture.planner().plan(&request).expect("plan");

    // Same canonical truth, rows written again in the reverse order.
    let mut reversed = fixture.standard_graph();
    reversed.reverse();
    fixture.publish(&reversed);
    let second = fixture.planner().plan(&request).expect("plan");
    assert_eq!(first, second);

    // Knowledge inserted in different orders under different uids.
    let labels: Vec<Vec<String>> = [false, true]
        .into_iter()
        .map(|reverse| {
            let fixture = Fixture::standard(&format!("order-knowledge-{reverse}"));
            let project = fixture.project();
            let mut rows = vec![("zeta", "a"), ("alpha", "b"), ("mid", "c")];
            if reverse {
                rows.reverse();
            }
            for (name, value) in &rows {
                project.insert_policy(&policy(name, name)).expect("policy");
                project
                    .insert_decision(&decision(name, value))
                    .expect("decision");
                project
                    .upsert_project_state(&state(name, value))
                    .expect("state");
            }
            let mut request = fixture.request(ProjectionIntent::ResumeHandoff, None);
            request.work_item = Some(fixture.work_item("order", &[]));
            for (name, _) in &rows {
                request.knowledge.decision_topics.insert((*name).to_owned());
                request.knowledge.state_keys.insert((*name).to_owned());
            }
            knowledge_labels(&fixture.planner().plan(&request).expect("plan"))
        })
        .collect();
    assert_eq!(labels[0], labels[1]);
}

#[test]
fn evidence_reached_twice_is_emitted_once_in_category_order() {
    let fixture = Fixture::standard("dedupe");
    let planner = fixture.planner();
    let shared = fixture.endpoint("src/shared.ts", "shared");
    let projection = planner
        .plan(&fixture.request(
            ProjectionIntent::Change(Some(ChangeKind::Structural(ImpactIntent::Rename))),
            Some(ProjectionTarget::Endpoint(shared.clone())),
        ))
        .expect("plan");

    // Feed the same facts back in again: direct answer + impact + target.
    let direct = planner
        .tests
        .traversal()
        .relations()
        .incoming(&shared, &[])
        .expect("incoming");
    let mut items = projection.evidence.clone();
    items.extend(projection.evidence.iter().cloned());
    items.extend(direct.confirmed.into_iter().map(EvidenceItem::Relation));
    let reordered = order(items);

    let identities = |items: &[EvidenceItem]| -> Vec<Vec<u8>> {
        items.iter().filter_map(|item| describe(item).3).collect()
    };
    let unique: BTreeSet<Vec<u8>> = identities(&reordered).into_iter().collect();
    assert_eq!(
        unique.len(),
        identities(&reordered).len(),
        "no identity twice"
    );
    assert_eq!(
        sources(&PreparedProjection {
            evidence: reordered.clone(),
            ..projection.clone()
        })
        .len(),
        1
    );
    let categories: Vec<u8> = reordered.iter().map(|item| describe(item).0).collect();
    assert!(
        categories.windows(2).all(|pair| pair[0] <= pair[1]),
        "{categories:?}"
    );
}

#[test]
fn overlapping_ranges_merge_before_read_and_disjoint_ones_do_not() {
    let resource = ResourceId::generate();
    let range = |start: usize, end: usize, role| PlannedSourceRange {
        resource,
        resource_revision: "rev".to_owned(),
        span: SourceSpan {
            start_byte: start,
            end_byte: end,
            start: crate::parser::SourcePoint {
                line: 0,
                column: start,
            },
            end: crate::parser::SourcePoint {
                line: 0,
                column: end,
            },
        },
        role,
        requirement: SourceRequirement::Required,
    };
    let merged = merge(&[
        range(50, 60, RangeRole::EvidenceSpan),
        range(10, 30, RangeRole::EvidenceSpan),
        range(20, 40, RangeRole::AnchorDeclaration),
        range(10, 30, RangeRole::EvidenceSpan),
    ]);
    let spans: Vec<(usize, usize, RangeRole)> = merged
        .iter()
        .map(|range| (range.span.start_byte, range.span.end_byte, range.role))
        .collect();
    assert_eq!(
        spans,
        [
            (10, 40, RangeRole::AnchorDeclaration),
            (50, 60, RangeRole::EvidenceSpan)
        ]
    );
}

// ------------------------------------------------------ semantics / wake

#[test]
fn persisted_semantic_truth_is_used_without_waking_a_backend() {
    use crate::{
        rust_semantic::tests_support::{self, ScriptedBackend},
        semantic_index::SemanticIndex,
    };
    let home = TestDir::create("semantic-home");
    let root = TestDir::create("semantic-root");
    tests_support::copy_tree(&tests_support::committed_fixture(), &root.0);
    let fixture = Fixture::init(home, root);
    let semantic = ManuallyDrop::new(tests_support::Fixture {
        base: fixture.paths.root.clone(),
        root: fixture.root.clone(),
    });
    assert_eq!(semantic.db_path(), fixture.paths.index_db);

    // The fleet publishes semantic truth ahead of time (the watcher's job).
    let main = "crates/app/src/main.rs";
    let runner = "crates/core/src/runner.rs";
    let index = SemanticIndex::open(&fixture.paths.index_db).expect("semantic index");
    let text = semantic.text(main);
    // The structural tier anchors the whole compound `use` specifier.
    let (start, end) = crate::evidence::list_unresolved_for_resource(
        index.connection(),
        fixture.resource(main).id,
    )
    .expect("gaps")
    .iter()
    .map(|gap| (gap.occurrence.start_byte, gap.occurrence.end_byte))
    .find(|(start, end)| text[*start..*end].contains("bp_core::runner::"))
    .expect("the use site");
    let backend = ScriptedBackend::loaded().with_definition(
        &semantic.uri(main),
        semantic.last_character(main, start, end),
        vec![semantic.module_location(runner)],
    );
    tests_support::refresh(&semantic, &index, &backend, main).expect("publish");
    let calls_before = backend.calls().len();

    let planner = fixture.planner();
    let target = fixture.file(runner);
    let projection = planner
        .plan(&fixture.request(
            ProjectionIntent::Impact(ChangeKind::Structural(ImpactIntent::ModuleMove)),
            Some(ProjectionTarget::Endpoint(target.clone())),
        ))
        .expect("plan");
    assert_eq!(backend.calls().len(), calls_before, "zero backend requests");
    assert!(
        relations(&projection)
            .iter()
            .any(|relation| relation.target == target && relation.source == fixture.file(main)),
        "the persisted semantic edge answers: {:?}",
        relations(&projection)
    );
}

/// Planner code, comments excluded.
fn planner_code() -> String {
    include_str!("../planner.rs")
        .split("\n#[cfg(test)]")
        .next()
        .expect("code")
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_planner_cannot_reach_a_backend_a_writer_or_a_second_walk() {
    let code = planner_code();
    for forbidden in [
        // semantic runtime / backends
        "Launcher",
        "Supervisor",
        "runtime::",
        "lsp::",
        "semantic_lifecycle",
        "_semantic::",
        "SemanticIndex",
        // refresh / sync / init / writes
        "BaselineScan",
        "refresh",
        "init_workspace",
        "rusqlite",
        "execute(",
        "INSERT",
        "UPDATE ",
        "DELETE ",
        ".start(",
        "update_progress",
        "promote",
        // a second traversal for tests
        "for_target",
        // task 7 / 8 / 9
        "max_items",
        "max_bytes",
        "max_tokens",
        "resume(",
        "raw_available",
        "delivered",
        "ledger",
        "fan_in",
        "confidence",
    ] {
        assert!(!code.contains(forbidden), "{forbidden}");
    }
}

#[test]
fn the_exact_blueprint_lookup_uses_the_unique_uid_index() {
    let fixture = Fixture::standard("eqp");
    let connection = rusqlite::Connection::open(&fixture.paths.project_db).expect("raw");
    let plan: Vec<String> = connection
        .prepare("EXPLAIN QUERY PLAN SELECT * FROM blueprint_application WHERE uid = ?1")
        .expect("prepare")
        .query_map([vec![0_u8; 16]], |row| row.get::<_, String>(3))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert!(
        plan.iter()
            .any(|line| line.contains("USING INDEX") && line.contains("autoindex")),
        "{plan:?}"
    );
}

/// The observations recorded in #20 for task 6, on the standard and the
/// broad fixture. Asserted, and printed for the record.
#[test]
fn planner_instrumentation_is_recorded() {
    let fixture = Fixture::standard("instrumentation");
    fixture
        .project()
        .insert_policy(&policy("applicable rule", "rule"))
        .expect("policy");
    let mine = fixture.work_item("measure", &[("src/shared.ts", WorkResourceRole::Target)]);
    let planner = fixture.planner();
    let shared = fixture.endpoint("src/shared.ts", "shared");
    let mut change = fixture.request(
        ProjectionIntent::Change(Some(ChangeKind::Structural(ImpactIntent::Rename))),
        Some(ProjectionTarget::Endpoint(shared.clone())),
    );
    change.work_item = Some(mine);
    for (label, request) in [
        (
            "LOCATE(Name run)",
            fixture.request(
                ProjectionIntent::Locate,
                symbol(SymbolName::Name("run".to_owned())),
            ),
        ),
        (
            "LOCATE",
            fixture.request(
                ProjectionIntent::Locate,
                symbol(SymbolName::Name("shared".to_owned())),
            ),
        ),
        (
            "UNDERSTAND",
            fixture.request(
                ProjectionIntent::Understand,
                Some(ProjectionTarget::Endpoint(shared.clone())),
            ),
        ),
        ("CHANGE(Rename)+WorkItem", change),
        (
            "IMPACT(Rename)",
            fixture.request(
                ProjectionIntent::Impact(ChangeKind::Structural(ImpactIntent::Rename)),
                Some(ProjectionTarget::Endpoint(shared.clone())),
            ),
        ),
    ] {
        let before = planner.stats();
        let started = std::time::Instant::now();
        let projection = planner.plan(&request).expect("plan");
        let wall = started.elapsed();
        let after = planner.stats();
        eprintln!(
            "TASK6 {label}: wall_us={} evidence={} gaps={} source_plan={} file_reads={} bytes={} \
             relation_queries={} impact_traversals={} knowledge_items={} work_snapshots={}",
            wall.as_micros(),
            projection.evidence.len(),
            projection.gaps.len(),
            projection.source_plan.len(),
            after.source_file_reads - before.source_file_reads,
            after.source_bytes - before.source_bytes,
            after.relation_queries - before.relation_queries,
            after.impact_traversals - before.impact_traversals,
            after.knowledge_items - before.knowledge_items,
            after.work_snapshots - before.work_snapshots,
        );
        assert!(after.source_file_reads - before.source_file_reads <= 1);
        assert!(after.impact_traversals - before.impact_traversals <= 1);
    }
    assert_eq!(planner.stats().plans, 5);
}

mod delivery;
mod economy;
mod query_surface;
