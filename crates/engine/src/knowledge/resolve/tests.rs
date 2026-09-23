//! #20 task 2 acceptance: scope layers, precedence per category, protected
//! layer, evidence categories, worktree isolation, minimal global
//! retrieval, and deterministic ordering.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
};

use brainprint_core::{BlueprintId, WorkItemId, WorkspaceId};
use rusqlite::Connection;

use std::collections::BTreeSet;

use super::super::*;
use super::FETCH_BOUND;
use crate::{
    init::init_workspace,
    paths::{GlobalPaths, WorkspacePaths},
    registry::GlobalRegistry,
};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "brainprint-resolve-{label}-{}-{sequence}",
            process::id()
        ));
        fs::create_dir_all(&path).expect("test directory should be created");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    dir: TestDir,
    global: GlobalKnowledgeStore,
    project: ProjectKnowledgeStore,
    workspace: WorkspaceKnowledgeStore,
    workspace_id: WorkspaceId,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let dir = TestDir::create(label);
        Self {
            global: GlobalKnowledgeStore::open(&dir.path().join("global.db")).expect("global"),
            project: ProjectKnowledgeStore::open(&dir.path().join("project.db")).expect("project"),
            workspace: WorkspaceKnowledgeStore::open(&dir.path().join("workspace.db"))
                .expect("workspace"),
            workspace_id: WorkspaceId::generate(),
            dir,
        }
    }

    fn sources(&self) -> KnowledgeSources<'_> {
        KnowledgeSources {
            global: &self.global,
            project: &self.project,
            workspace: Some(&self.workspace),
        }
    }

    fn base(&self) -> ApplicabilityContext {
        ApplicabilityContext::base(Some(self.workspace_id))
    }

    fn ws(&self) -> KnowledgeScope {
        KnowledgeScope::workspace(self.workspace_id)
    }

    fn resolve(&self, request: &ResolveRequest) -> ResolvedKnowledge {
        resolve(&self.sources(), request).expect("resolve")
    }

    fn resolve_base(&self) -> ResolvedKnowledge {
        self.resolve(&ResolveRequest::new(self.base()))
    }

    fn raw_project(&self) -> Connection {
        Connection::open(self.dir.path().join("project.db")).expect("raw project.db")
    }
}

fn provenance(source_kind: SourceKind) -> Provenance {
    Provenance::new(source_kind).with_locator("issue#20")
}

fn policy(scope: KnowledgeScope, key: Option<&str>, title: &str) -> NewPolicy {
    NewPolicy {
        scope,
        policy_key: key.map(str::to_owned),
        title: title.to_owned(),
        rule_text: format!("rule {title}"),
        structured_rule: None,
        protection_class: ProtectionClass::Normal,
        priority_class: PriorityClass::Default,
        provenance: provenance(SourceKind::UserExplicit),
    }
}

fn protected(
    scope: KnowledgeScope,
    key: &str,
    title: &str,
    class: ProtectionClass,
    source_kind: SourceKind,
) -> NewPolicy {
    NewPolicy {
        protection_class: class,
        provenance: provenance(source_kind),
        ..policy(scope, Some(key), title)
    }
}

fn decision(scope: KnowledgeScope, topic: &str, chosen: &str) -> NewDecision {
    NewDecision {
        scope,
        topic: topic.to_owned(),
        chosen_summary: chosen.to_owned(),
        rationale: "because".to_owned(),
        provenance: provenance(SourceKind::UserExplicit),
    }
}

fn preference(scope: KnowledgeScope, key: &str, value: &str) -> NewUserPreference {
    NewUserPreference {
        scope,
        preference_key: key.to_owned(),
        value: TypedValue::Text(value.to_owned()),
        provenance: provenance(SourceKind::UserExplicit),
    }
}

fn directive(
    id: &str,
    target: DirectiveTarget,
    key: &str,
    scope: KnowledgeScope,
) -> RequestDirective {
    RequestDirective {
        id: id.to_owned(),
        target,
        subject_key: key.to_owned(),
        scope,
        summary: format!("{id} says so"),
    }
}

fn blueprint(scope: KnowledgeScope, title: &str) -> NewBlueprint {
    NewBlueprint {
        scope,
        blueprint_key: None,
        title: title.to_owned(),
        intent: format!("{title} intent"),
        definition: BlueprintDefinition {
            constraints: vec!["tests may be skipped".to_owned()],
            ..BlueprintDefinition::default()
        },
        status: BlueprintStatus::Active,
        version: None,
        provenance: provenance(SourceKind::UserExplicit),
    }
}

fn application(
    owner: BlueprintOwnerKind,
    uid: BlueprintId,
    summary: &str,
) -> NewBlueprintApplication {
    NewBlueprintApplication {
        blueprint: BlueprintRef { owner, uid },
        scope: KnowledgeScope::project(),
        application_summary: summary.to_owned(),
        provenance: provenance(SourceKind::UserExplicit),
    }
}

fn state(key: &str, scope: KnowledgeScope, value: &str) -> ProjectStateUpdate {
    ProjectStateUpdate {
        key: key.to_owned(),
        scope,
        value: TypedValue::Text(value.to_owned()),
        status: ProjectStateStatus::Current,
        provenance: Provenance::new(SourceKind::Observed),
    }
}

fn domain(key: &str) -> KnowledgeScope {
    KnowledgeScope::keyed(ScopeKind::Domain, key).expect("domain scope")
}

fn titles(items: &[Resolved<Policy>]) -> Vec<&str> {
    items
        .iter()
        .map(|entry| entry.item.title.as_str())
        .collect()
}

fn chosen(items: &[Resolved<Decision>]) -> Vec<&str> {
    items
        .iter()
        .map(|entry| entry.item.chosen_summary.as_str())
        .collect()
}

fn shadow_reasons(result: &ResolvedKnowledge) -> Vec<(String, ResolutionReason)> {
    result
        .shadowed
        .iter()
        .map(|entry| {
            let label = match &entry.item {
                ShadowedItem::Policy(item) => item.title.clone(),
                ShadowedItem::Decision(item) => item.chosen_summary.clone(),
                ShadowedItem::Preference(item) => item.value.to_json(),
            };
            (label, entry.reason)
        })
        .collect()
}

fn conflict_kinds(result: &ResolvedKnowledge) -> Vec<ConflictKind> {
    result
        .conflicts
        .iter()
        .map(|conflict| conflict.kind)
        .collect()
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
        .expect("git should be runnable");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ------------------------------------------------------ applicability (1-3)

#[test]
fn base_context_orders_global_project_workspace() {
    let workspace = WorkspaceId::generate();
    let context = ApplicabilityContext::base(Some(workspace));
    assert_eq!(
        context.layers(),
        [
            vec![KnowledgeScope::global()],
            vec![KnowledgeScope::project()],
            vec![KnowledgeScope::workspace(workspace)],
        ]
    );
    assert_eq!(context.layer_of(&KnowledgeScope::global()), Some(0));
    assert_eq!(
        context.layer_of(&KnowledgeScope::workspace(workspace)),
        Some(2)
    );
    assert_eq!(
        context.layer_of(&KnowledgeScope::workspace(WorkspaceId::generate())),
        None
    );
    assert_eq!(ApplicabilityContext::base(None).layers().len(), 2);
    assert_eq!(
        ApplicabilityContext::new(context.layers().to_vec()).expect("valid"),
        context
    );
}

#[test]
fn malformed_context_is_rejected() {
    let g = KnowledgeScope::global;
    let p = KnowledgeScope::project;
    let w = || KnowledgeScope::workspace(WorkspaceId::generate());
    let malformed = [
        vec![vec![g()], vec![p()], vec![domain("db"), domain("db")]],
        vec![vec![g()], vec![p()], vec![domain("db")], vec![domain("db")]],
        vec![vec![g()], vec![g()]],
        vec![vec![g()], vec![]],
        vec![vec![g(), p()]],
        vec![vec![p()], vec![g()]],
        vec![vec![g()], vec![domain("db")], vec![p()]],
        vec![vec![g()], vec![p(), domain("db")]],
        vec![vec![g()], vec![p()], vec![w()], vec![w()]],
        vec![vec![g()], vec![p()], vec![w(), w()]],
    ];
    for layers in malformed {
        assert!(
            matches!(
                ApplicabilityContext::new(layers.clone()),
                Err(ResolveError::InvalidContext(_))
            ),
            "{layers:?} must be rejected"
        );
    }
    assert!(
        ApplicabilityContext::base(None)
            .with_layer(vec![KnowledgeScope::project()])
            .is_err()
    );
    // Keyed GLOBAL and a non-WorkspaceID WORKSPACE cannot even be built.
    assert!(KnowledgeScope::keyed(ScopeKind::Global, "x").is_err());
    assert!(KnowledgeScope::keyed(ScopeKind::Workspace, "not-a-workspace-id").is_err());
}

#[test]
fn exact_domain_scope_applies_only_when_requested() {
    let fx = Fixture::new("domain");
    fx.global
        .insert_user_policy(&policy(domain("database"), None, "global db"))
        .expect("policy");
    fx.global
        .insert_user_policy(&policy(domain("frontend"), None, "global frontend"))
        .expect("policy");
    fx.project
        .insert_policy(&policy(domain("database"), None, "project db"))
        .expect("policy");

    assert!(fx.resolve_base().applied_policies.is_empty());

    let context = fx.base().with_layer(vec![domain("database")]).expect("ctx");
    let result = fx.resolve(&ResolveRequest::new(context));
    assert_eq!(
        titles(&result.applied_policies),
        ["global db", "project db"]
    );
    assert!(result.applied_policies.iter().all(|entry| entry.layer == 3));
}

// ----------------------------------------------------------- policy (4-11)

#[test]
fn project_policy_overrides_same_key_global_user_policy() {
    let fx = Fixture::new("project-over-global");
    fx.global
        .insert_user_policy(&policy(KnowledgeScope::global(), Some("tests"), "global"))
        .expect("policy");
    fx.project
        .insert_policy(&policy(KnowledgeScope::project(), Some("tests"), "project"))
        .expect("policy");
    let result = fx.resolve_base();
    assert_eq!(titles(&result.applied_policies), ["project"]);
    assert_eq!(
        result.applied_policies[0].reason,
        ResolutionReason::SelectedPolicy
    );
    assert_eq!(
        shadow_reasons(&result),
        [("global".to_owned(), ResolutionReason::ShadowedByProjectTier)]
    );

    // Project tier wins even over a more specific global DOMAIN Policy.
    fx.global
        .insert_user_policy(&policy(domain("db"), Some("tests"), "global domain"))
        .expect("policy");
    let context = fx.base().with_layer(vec![domain("db")]).expect("ctx");
    assert_eq!(
        titles(&fx.resolve(&ResolveRequest::new(context)).applied_policies),
        ["project"]
    );
}

#[test]
fn workspace_policy_overrides_project_policy() {
    let fx = Fixture::new("workspace-over-project");
    fx.global
        .insert_user_policy(&policy(KnowledgeScope::global(), Some("tests"), "global"))
        .expect("policy");
    fx.project
        .insert_policy(&policy(KnowledgeScope::project(), Some("tests"), "project"))
        .expect("policy");
    fx.project
        .insert_policy(&policy(fx.ws(), Some("tests"), "workspace"))
        .expect("policy");
    let result = fx.resolve_base();
    assert_eq!(titles(&result.applied_policies), ["workspace"]);
    assert_eq!(result.applied_policies[0].layer, 2);
    assert_eq!(
        shadow_reasons(&result),
        [
            ("global".to_owned(), ResolutionReason::ShadowedByProjectTier),
            (
                "project".to_owned(),
                ResolutionReason::ShadowedByMoreSpecificScope
            ),
        ]
    );
}

#[test]
fn lower_specificity_keyed_policy_is_not_the_selected_winner() {
    let fx = Fixture::new("lower-specificity");
    let package = KnowledgeScope::keyed(ScopeKind::Package, "engine").expect("package");
    fx.project
        .insert_policy(&policy(KnowledgeScope::project(), Some("style"), "broad"))
        .expect("policy");
    fx.project
        .insert_policy(&policy(package.clone(), Some("style"), "narrow"))
        .expect("policy");
    let context = fx.base().with_layer(vec![package]).expect("ctx");
    let result = fx.resolve(&ResolveRequest::new(context));
    assert_eq!(titles(&result.applied_policies), ["narrow"]);
    assert!(
        !titles(&result.applied_policies).contains(&"broad"),
        "the lower layer is shadowed, not selected"
    );
    // Without the package evidence the package Policy does not apply.
    assert_eq!(titles(&fx.resolve_base().applied_policies), ["broad"]);
}

#[test]
fn same_layer_policies_stay_additive_without_invented_conflict() {
    let fx = Fixture::new("additive");
    fx.project
        .insert_policy(&policy(
            KnowledgeScope::project(),
            Some("tests"),
            "run unit tests",
        ))
        .expect("policy");
    fx.project
        .insert_policy(&policy(
            KnowledgeScope::project(),
            Some("tests"),
            "never skip tests",
        ))
        .expect("policy");
    let result = fx.resolve_base();
    assert_eq!(
        titles(&result.applied_policies),
        ["never skip tests", "run unit tests"]
    );
    assert!(result.conflicts.is_empty());
    assert!(result.shadowed.is_empty());
}

#[test]
fn unkeyed_policy_remains_applicable() {
    let fx = Fixture::new("unkeyed");
    fx.project
        .insert_policy(&policy(KnowledgeScope::project(), None, "unkeyed"))
        .expect("policy");
    fx.project
        .insert_policy(&policy(fx.ws(), Some("tests"), "keyed workspace"))
        .expect("policy");
    let result = fx.resolve(&ResolveRequest {
        directives: vec![directive(
            "d1",
            DirectiveTarget::Policy,
            "tests",
            KnowledgeScope::project(),
        )],
        ..ResolveRequest::new(fx.base())
    });
    assert_eq!(titles(&result.applied_policies), ["unkeyed"]);
    assert_eq!(
        result.applied_policies[0].reason,
        ResolutionReason::UnkeyedPolicy
    );
}

#[test]
fn disabled_and_superseded_policies_are_excluded() {
    let fx = Fixture::new("excluded-policy");
    let disabled = fx
        .project
        .insert_policy(&policy(KnowledgeScope::project(), None, "disabled"))
        .expect("policy");
    fx.project
        .set_policy_status(disabled.uid, PolicyStatus::Disabled)
        .expect("disable");
    let old = fx
        .project
        .insert_policy(&policy(KnowledgeScope::project(), Some("k"), "old"))
        .expect("policy");
    let new = fx
        .project
        .insert_policy(&policy(KnowledgeScope::project(), Some("k"), "new"))
        .expect("policy");
    fx.project
        .supersede_policy(new.uid, old.uid)
        .expect("supersede");
    let old_global = fx
        .global
        .insert_user_policy(&policy(KnowledgeScope::global(), None, "global old"))
        .expect("policy");
    fx.global
        .set_user_policy_status(old_global.uid, PolicyStatus::Disabled)
        .expect("disable");

    let result = fx.resolve_base();
    assert_eq!(titles(&result.applied_policies), ["new"]);
    assert!(
        result.shadowed.is_empty(),
        "inactive rows are not candidates"
    );
}

#[test]
fn request_directive_overrides_ordinary_same_subject_policy() {
    let fx = Fixture::new("directive-policy");
    fx.project
        .insert_policy(&policy(fx.ws(), Some("tests"), "run all tests"))
        .expect("policy");
    fx.project
        .insert_policy(&policy(KnowledgeScope::project(), Some("other"), "other"))
        .expect("policy");
    let result = fx.resolve(&ResolveRequest {
        directives: vec![directive(
            "d1",
            DirectiveTarget::Policy,
            "tests",
            KnowledgeScope::project(),
        )],
        ..ResolveRequest::new(fx.base())
    });
    assert_eq!(titles(&result.applied_policies), ["other"]);
    assert_eq!(result.request_directives.len(), 1);
    assert_eq!(result.request_directives[0].origin, Origin::Request);
    assert_eq!(
        result.request_directives[0].reason,
        ResolutionReason::RequestExplicit
    );
    assert_eq!(
        shadow_reasons(&result),
        [(
            "run all tests".to_owned(),
            ResolutionReason::ShadowedByRequest
        )]
    );
}

#[test]
fn request_directive_does_not_mutate_durable_policy() {
    let fx = Fixture::new("directive-readonly");
    let stored = fx
        .project
        .insert_policy(&policy(KnowledgeScope::project(), Some("tests"), "tests"))
        .expect("policy");
    let snapshot = |fx: &Fixture| -> Vec<(Vec<u8>, String, String)> {
        let raw = fx.raw_project();
        let mut statement = raw
            .prepare("SELECT uid, status, updated_at FROM policy ORDER BY id")
            .expect("prepare");
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    };
    let before = snapshot(&fx);
    fx.resolve(&ResolveRequest {
        directives: vec![directive(
            "d1",
            DirectiveTarget::Policy,
            "tests",
            KnowledgeScope::project(),
        )],
        ..ResolveRequest::new(fx.base())
    });
    assert_eq!(snapshot(&fx), before);
    assert_eq!(
        fx.project.get_policy(stored.uid).expect("get"),
        Some(stored)
    );
    assert_eq!(titles(&fx.resolve_base().applied_policies), ["tests"]);
}

#[test]
fn malformed_directives_are_rejected() {
    let fx = Fixture::new("directive-invalid");
    let foreign = KnowledgeScope::workspace(WorkspaceId::generate());
    let cases = [
        vec![directive("d1", DirectiveTarget::Policy, "k", foreign)],
        vec![
            directive(
                "d1",
                DirectiveTarget::Policy,
                "k",
                KnowledgeScope::project(),
            ),
            directive(
                "d1",
                DirectiveTarget::Decision,
                "k",
                KnowledgeScope::project(),
            ),
        ],
        vec![
            directive(
                "d1",
                DirectiveTarget::Policy,
                "k",
                KnowledgeScope::project(),
            ),
            directive("d2", DirectiveTarget::Policy, "k", fx.ws()),
        ],
        vec![directive(
            "",
            DirectiveTarget::Policy,
            "k",
            KnowledgeScope::project(),
        )],
    ];
    for directives in cases {
        let request = ResolveRequest {
            directives,
            ..ResolveRequest::new(fx.base())
        };
        assert!(matches!(
            resolve(&fx.sources(), &request),
            Err(ResolveError::InvalidRequest(_))
        ));
    }
}

// ------------------------------------------------------- protected (12-16)

fn protected_survives(class: ProtectionClass, label: &str) {
    let fx = Fixture::new(label);
    fx.global
        .insert_user_policy(&protected(
            KnowledgeScope::global(),
            "secrets",
            "never print secrets",
            class,
            SourceKind::UserExplicit,
        ))
        .expect("protected");
    fx.project
        .insert_policy(&policy(fx.ws(), Some("secrets"), "project may print"))
        .expect("policy");
    fx.project
        .insert_decision(&decision(fx.ws(), "secrets", "print them"))
        .expect("decision");
    fx.global
        .insert_user_preference(&preference(KnowledgeScope::global(), "secrets", "show"))
        .expect("preference");
    fx.project
        .insert_blueprint(&blueprint(KnowledgeScope::project(), "bp"))
        .and_then(|bp| {
            fx.project.insert_blueprint_application(&application(
                BlueprintOwnerKind::Project,
                bp.uid,
                "app",
            ))
        })
        .expect("blueprint");
    let item = fx
        .workspace
        .create_work_item(&NewWorkItem {
            source_kind: WorkItemSourceKind::Issue,
            source_ref: None,
            title: None,
            goal: "g".to_owned(),
        })
        .expect("item");

    let result = fx.resolve(&ResolveRequest {
        directives: vec![directive(
            "d1",
            DirectiveTarget::Decision,
            "secrets",
            KnowledgeScope::project(),
        )],
        decision_topics: vec!["secrets".to_owned()],
        preference_keys: vec!["secrets".to_owned()],
        state_keys: vec!["secrets".to_owned()],
        include_blueprints: true,
        work_item: Some(item.uid),
        ..ResolveRequest::new(fx.base())
    });
    assert_eq!(
        titles(&result.protected_constraints),
        ["never print secrets"]
    );
    assert_eq!(result.protected_constraints[0].item.protection_class, class);
    assert_eq!(
        result.protected_constraints[0].reason,
        ResolutionReason::ProtectedConstraint
    );
    // The ordinary same-key project Policy is still resolved normally.
    assert_eq!(titles(&result.applied_policies), ["project may print"]);
    assert!(result.conflicts.is_empty());
}

#[test]
fn eligible_protected_privacy_policy_always_survives() {
    protected_survives(ProtectionClass::ProtectedPrivacy, "protected-privacy");
}

#[test]
fn eligible_protected_security_policy_always_survives() {
    protected_survives(ProtectionClass::ProtectedSecurity, "protected-security");
}

#[test]
fn directive_weakening_protected_policy_is_a_conflict() {
    let fx = Fixture::new("protected-override");
    let guard = fx
        .project
        .insert_policy(&protected(
            KnowledgeScope::project(),
            "credentials",
            "no credentials in logs",
            ProtectionClass::ProtectedSecurity,
            SourceKind::AuthoritativeArtifact,
        ))
        .expect("protected");
    fx.project
        .insert_policy(&policy(
            KnowledgeScope::project(),
            Some("credentials"),
            "ordinary",
        ))
        .expect("policy");
    let result = fx.resolve(&ResolveRequest {
        directives: vec![directive(
            "relax",
            DirectiveTarget::Policy,
            "credentials",
            fx.ws(),
        )],
        ..ResolveRequest::new(fx.base())
    });
    assert_eq!(
        titles(&result.protected_constraints),
        ["no credentials in logs"]
    );
    assert!(result.request_directives.is_empty(), "not applied");
    assert_eq!(titles(&result.applied_policies), ["ordinary"]);
    assert_eq!(result.conflicts.len(), 1);
    let conflict = &result.conflicts[0];
    assert_eq!(conflict.kind, ConflictKind::ProtectedOverrideRejected);
    assert_eq!(conflict.subject.as_deref(), Some("credentials"));
    let ids: Vec<_> = conflict.involved.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, [guard.uid.to_string().as_str(), "relax"]);
    assert_eq!(conflict.involved[0].scope, KnowledgeScope::project());
    assert_eq!(
        conflict.involved[0].source_kind,
        Some(SourceKind::AuthoritativeArtifact)
    );
    assert_eq!(conflict.involved[1].origin, Origin::Request);
    assert_eq!(conflict.involved[1].layer, 2);
}

fn untrusted_protected_cannot_self_elevate(source_kind: SourceKind, label: &str) {
    let fx = Fixture::new(label);
    let row = fx
        .project
        .insert_policy(&protected(
            KnowledgeScope::project(),
            "network",
            "self-declared guard",
            ProtectionClass::ProtectedSecurity,
            source_kind,
        ))
        .expect("storage accepts it (task 4 owns promotion)");
    fx.global
        .insert_user_policy(&protected(
            KnowledgeScope::global(),
            "telemetry",
            "self-declared privacy",
            ProtectionClass::ProtectedPrivacy,
            source_kind,
        ))
        .expect("storage accepts it");
    let result = fx.resolve(&ResolveRequest {
        directives: vec![directive(
            "d1",
            DirectiveTarget::Policy,
            "network",
            KnowledgeScope::project(),
        )],
        ..ResolveRequest::new(fx.base())
    });
    assert!(result.protected_constraints.is_empty());
    assert!(result.applied_policies.is_empty());
    assert_eq!(
        conflict_kinds(&result),
        [
            ConflictKind::InvalidProtectedProvenance,
            ConflictKind::InvalidProtectedProvenance
        ]
    );
    let network = &result.conflicts[0];
    assert_eq!(network.subject.as_deref(), Some("network"));
    assert_eq!(network.involved[0].id, row.uid.to_string());
    assert_eq!(network.involved[0].source_kind, Some(source_kind));
    assert_eq!(network.involved[0].status, Some("ACTIVE"));
    assert_eq!(
        result.request_directives.len(),
        1,
        "no protection to reject it"
    );
}

#[test]
fn agent_reported_protected_policy_cannot_self_elevate() {
    untrusted_protected_cannot_self_elevate(SourceKind::AgentReported, "agent-protected");
}

#[test]
fn observed_protected_policy_cannot_self_elevate() {
    untrusted_protected_cannot_self_elevate(SourceKind::Observed, "observed-protected");
}

// -------------------------------------------------------- decision (17-21)

#[test]
fn more_specific_decision_wins_lower_scope() {
    let fx = Fixture::new("decision-specific");
    fx.project
        .insert_decision(&decision(KnowledgeScope::project(), "db", "sqlite"))
        .expect("decision");
    fx.project
        .insert_decision(&decision(fx.ws(), "db", "postgres"))
        .expect("decision");
    let request = ResolveRequest {
        decision_topics: vec!["db".to_owned()],
        ..ResolveRequest::new(fx.base())
    };
    let result = fx.resolve(&request);
    assert_eq!(chosen(&result.active_decisions), ["postgres"]);
    assert_eq!(
        result.active_decisions[0].reason,
        ResolutionReason::ResolvedDecision
    );
    assert_eq!(
        shadow_reasons(&result),
        [(
            "sqlite".to_owned(),
            ResolutionReason::ShadowedByMoreSpecificScope
        )]
    );
    // Another Workspace's context does not see the workspace Decision.
    let other = ResolveRequest {
        context: ApplicabilityContext::base(Some(WorkspaceId::generate())),
        ..request
    };
    let sources = KnowledgeSources {
        workspace: None,
        ..fx.sources()
    };
    let result = resolve(&sources, &other).expect("resolve");
    assert_eq!(chosen(&result.active_decisions), ["sqlite"]);
}

#[test]
fn same_layer_different_decisions_conflict_without_winner() {
    let fx = Fixture::new("decision-conflict");
    let a = fx
        .project
        .insert_decision(&decision(KnowledgeScope::project(), "db", "sqlite"))
        .expect("decision");
    let b = fx
        .project
        .insert_decision(&decision(KnowledgeScope::project(), "db", "postgres"))
        .expect("decision");
    fx.project
        .insert_decision(&decision(KnowledgeScope::project(), "lang", "rust"))
        .expect("decision");
    fx.project
        .insert_decision(&decision(KnowledgeScope::project(), "lang", "rust"))
        .expect("duplicate meaning");
    let result = fx.resolve(&ResolveRequest {
        decision_topics: vec!["db".to_owned(), "lang".to_owned()],
        ..ResolveRequest::new(fx.base())
    });
    assert_eq!(chosen(&result.active_decisions), ["rust", "rust"]);
    assert_eq!(
        conflict_kinds(&result),
        [ConflictKind::SameSpecificityDecision]
    );
    let conflict = &result.conflicts[0];
    assert_eq!(conflict.subject.as_deref(), Some("db"));
    let mut expected = vec![a.uid.to_string(), b.uid.to_string()];
    expected.sort();
    let ids: Vec<_> = conflict.involved.iter().map(|e| e.id.clone()).collect();
    assert_eq!(ids, expected);
}

#[test]
fn timestamps_do_not_resolve_decision_conflict() {
    let fx = Fixture::new("decision-timestamp");
    let old = fx
        .project
        .insert_decision(&decision(KnowledgeScope::project(), "db", "sqlite"))
        .expect("decision");
    fx.project
        .insert_decision(&decision(KnowledgeScope::project(), "db", "postgres"))
        .expect("decision");
    fx.raw_project()
        .execute(
            "UPDATE decision SET created_at = '9999999999999', updated_at = '9999999999999' \
             WHERE uid = ?1",
            [old.uid.to_bytes().to_vec()],
        )
        .expect("age one row");
    let result = fx.resolve(&ResolveRequest {
        decision_topics: vec!["db".to_owned()],
        ..ResolveRequest::new(fx.base())
    });
    assert!(result.active_decisions.is_empty());
    assert_eq!(
        conflict_kinds(&result),
        [ConflictKind::SameSpecificityDecision]
    );
}

#[test]
fn source_kind_does_not_resolve_ordinary_conflict() {
    let fx = Fixture::new("decision-source-kind");
    fx.project
        .insert_decision(&NewDecision {
            provenance: provenance(SourceKind::UserExplicit),
            ..decision(KnowledgeScope::project(), "db", "sqlite")
        })
        .expect("decision");
    fx.project
        .insert_decision(&NewDecision {
            provenance: provenance(SourceKind::AgentReported),
            ..decision(KnowledgeScope::project(), "db", "postgres")
        })
        .expect("decision");
    fx.project
        .insert_policy(&NewPolicy {
            provenance: provenance(SourceKind::AgentReported),
            ..policy(KnowledgeScope::project(), Some("fmt"), "agent fmt")
        })
        .expect("policy");
    fx.project
        .insert_policy(&policy(KnowledgeScope::project(), Some("fmt"), "user fmt"))
        .expect("policy");
    let result = fx.resolve(&ResolveRequest {
        decision_topics: vec!["db".to_owned()],
        ..ResolveRequest::new(fx.base())
    });
    assert!(result.active_decisions.is_empty());
    assert_eq!(
        conflict_kinds(&result),
        [ConflictKind::SameSpecificityDecision]
    );
    assert_eq!(titles(&result.applied_policies), ["agent fmt", "user fmt"]);
}

#[test]
fn superseded_and_reversed_decisions_are_excluded() {
    let fx = Fixture::new("decision-lineage");
    let old = fx
        .project
        .insert_decision(&decision(KnowledgeScope::project(), "db", "sqlite"))
        .expect("decision");
    let new = fx
        .project
        .insert_decision(&decision(KnowledgeScope::project(), "db", "postgres"))
        .expect("decision");
    fx.project
        .supersede_decision(new.uid, old.uid)
        .expect("supersede");
    let wrong = fx
        .project
        .insert_decision(&decision(KnowledgeScope::project(), "cache", "redis"))
        .expect("decision");
    let undo = fx
        .project
        .insert_decision(&decision(KnowledgeScope::project(), "cache", "none"))
        .expect("decision");
    fx.project
        .reverse_decision(undo.uid, wrong.uid)
        .expect("reverse");
    let result = fx.resolve(&ResolveRequest {
        decision_topics: vec!["db".to_owned(), "cache".to_owned()],
        ..ResolveRequest::new(fx.base())
    });
    assert_eq!(chosen(&result.active_decisions), ["none", "postgres"]);
    assert!(result.conflicts.is_empty());
    assert!(result.shadowed.is_empty());
}

// ------------------------------------------------------ preference (22-24)

#[test]
fn project_decision_shadows_same_key_global_preference() {
    let fx = Fixture::new("decision-over-preference");
    let python = fx
        .global
        .insert_user_preference(&preference(KnowledgeScope::global(), "language", "Python"))
        .expect("preference");
    fx.project
        .insert_decision(&decision(KnowledgeScope::project(), "language", "Rust"))
        .expect("decision");
    let result = fx.resolve(&ResolveRequest {
        preference_keys: vec!["language".to_owned()],
        ..ResolveRequest::new(fx.base())
    });
    assert_eq!(chosen(&result.active_decisions), ["Rust"]);
    assert!(result.applied_preferences.is_empty());
    assert_eq!(
        shadow_reasons(&result),
        [(
            "\"Python\"".to_owned(),
            ResolutionReason::ShadowedByProjectDecision
        )]
    );
    assert_eq!(
        fx.global
            .get_user_preference(python.uid)
            .expect("get")
            .map(|p| p.status),
        Some(PreferenceStatus::Active),
        "shadowing does not disable the Preference"
    );

    // Without a same-topic Decision the Preference applies.
    let result = fx.resolve(&ResolveRequest {
        preference_keys: vec!["editor".to_owned(), "language".to_owned()],
        ..ResolveRequest::new(ApplicabilityContext::base(None))
    });
    assert_eq!(result.applied_preferences.len(), 0);
    fx.global
        .insert_user_preference(&preference(KnowledgeScope::global(), "editor", "helix"))
        .expect("preference");
    let result = fx.resolve(&ResolveRequest {
        preference_keys: vec!["editor".to_owned()],
        ..ResolveRequest::new(fx.base())
    });
    assert_eq!(result.applied_preferences.len(), 1);
    assert_eq!(
        result.applied_preferences[0].reason,
        ResolutionReason::AppliedPreference
    );
}

#[test]
fn unrelated_user_preference_is_not_retrieved() {
    let fx = Fixture::new("preference-minimal");
    fx.global
        .insert_user_preference(&preference(KnowledgeScope::global(), "editor", "helix"))
        .expect("preference");
    fx.global
        .insert_user_preference(&preference(KnowledgeScope::global(), "language", "Python"))
        .expect("preference");
    assert!(fx.resolve_base().applied_preferences.is_empty());
    let result = fx.resolve(&ResolveRequest {
        preference_keys: vec!["language".to_owned()],
        ..ResolveRequest::new(fx.base())
    });
    let keys: Vec<_> = result
        .applied_preferences
        .iter()
        .map(|entry| entry.item.preference_key.as_str())
        .collect();
    assert_eq!(keys, ["language"]);
    assert!(result.shadowed.is_empty());
}

#[test]
fn preference_cannot_break_a_conflicted_project_decision() {
    let fx = Fixture::new("preference-tie");
    fx.global
        .insert_user_preference(&preference(KnowledgeScope::global(), "language", "Rust"))
        .expect("preference");
    fx.project
        .insert_decision(&decision(KnowledgeScope::project(), "language", "Rust"))
        .expect("decision");
    fx.project
        .insert_decision(&decision(KnowledgeScope::project(), "language", "Go"))
        .expect("decision");
    let result = fx.resolve(&ResolveRequest {
        preference_keys: vec!["language".to_owned()],
        ..ResolveRequest::new(fx.base())
    });
    assert!(result.active_decisions.is_empty());
    assert!(result.applied_preferences.is_empty());
    assert_eq!(
        conflict_kinds(&result),
        [ConflictKind::SameSpecificityDecision]
    );
    assert_eq!(
        shadow_reasons(&result),
        [(
            "\"Rust\"".to_owned(),
            ResolutionReason::ShadowedByProjectDecision
        )]
    );
}

#[test]
fn preference_specificity_and_same_layer_conflict() {
    let fx = Fixture::new("preference-layers");
    fx.global
        .insert_user_preference(&preference(KnowledgeScope::global(), "indent", "4"))
        .expect("preference");
    fx.global
        .insert_user_preference(&preference(domain("web"), "indent", "2"))
        .expect("preference");
    fx.global
        .insert_user_preference(&preference(KnowledgeScope::global(), "quote", "single"))
        .expect("preference");
    fx.global
        .insert_user_preference(&preference(KnowledgeScope::global(), "quote", "double"))
        .expect("preference");
    let context = fx.base().with_layer(vec![domain("web")]).expect("ctx");
    let result = fx.resolve(&ResolveRequest {
        preference_keys: vec!["indent".to_owned(), "quote".to_owned()],
        ..ResolveRequest::new(context)
    });
    assert_eq!(result.applied_preferences.len(), 1);
    assert_eq!(
        result.applied_preferences[0].item.value,
        TypedValue::Text("2".into())
    );
    assert_eq!(
        conflict_kinds(&result),
        [ConflictKind::SameSpecificityPreference]
    );
}

// ------------------------------------------------------- blueprint (25-28)

#[test]
fn blueprint_application_resolves_global_owner() {
    let fx = Fixture::new("blueprint-global");
    let reusable = fx
        .global
        .insert_blueprint(&blueprint(KnowledgeScope::global(), "reusable"))
        .expect("blueprint");
    fx.project
        .insert_blueprint_application(&application(
            BlueprintOwnerKind::Global,
            reusable.uid,
            "uses reusable",
        ))
        .expect("application");
    assert!(
        fx.resolve_base().blueprint_evidence.is_empty(),
        "only on request"
    );
    let result = fx.resolve(&ResolveRequest {
        include_blueprints: true,
        ..ResolveRequest::new(fx.base())
    });
    assert_eq!(result.blueprint_evidence.len(), 1);
    let evidence = &result.blueprint_evidence[0];
    assert_eq!(evidence.reason, ResolutionReason::BlueprintEvidence);
    assert_eq!(
        evidence.item.definition,
        BlueprintDefinitionState::Available(Box::new(reusable))
    );
}

#[test]
fn blueprint_application_resolves_project_owner() {
    let fx = Fixture::new("blueprint-project");
    let local = fx
        .project
        .insert_blueprint(&blueprint(KnowledgeScope::project(), "local"))
        .expect("blueprint");
    fx.project
        .insert_blueprint_application(&application(
            BlueprintOwnerKind::Project,
            local.uid,
            "uses local",
        ))
        .expect("application");
    let result = fx.resolve(&ResolveRequest {
        include_blueprints: true,
        ..ResolveRequest::new(fx.base())
    });
    assert_eq!(
        result.blueprint_evidence[0].item.definition,
        BlueprintDefinitionState::Available(Box::new(local))
    );
}

#[test]
fn missing_or_retired_blueprint_definition_is_an_explicit_gap() {
    let fx = Fixture::new("blueprint-gap");
    fx.project
        .insert_blueprint_application(&application(
            BlueprintOwnerKind::Global,
            BlueprintId::generate(),
            "a missing",
        ))
        .expect("application");
    let retired = fx
        .global
        .insert_blueprint(&blueprint(KnowledgeScope::global(), "old"))
        .expect("blueprint");
    fx.global
        .set_blueprint_status(retired.uid, BlueprintStatus::Retired)
        .expect("retire");
    fx.project
        .insert_blueprint_application(&application(
            BlueprintOwnerKind::Global,
            retired.uid,
            "b retired",
        ))
        .expect("application");
    let gone = fx
        .project
        .insert_blueprint_application(&application(
            BlueprintOwnerKind::Global,
            retired.uid,
            "c gone",
        ))
        .expect("application");
    fx.project
        .set_blueprint_application_status(gone.uid, BlueprintApplicationStatus::Retired)
        .expect("retire application");
    let result = fx.resolve(&ResolveRequest {
        include_blueprints: true,
        ..ResolveRequest::new(fx.base())
    });
    let states: Vec<_> = result
        .blueprint_evidence
        .iter()
        .map(|e| {
            (
                e.item.application.application_summary.as_str(),
                &e.item.definition,
            )
        })
        .collect();
    assert_eq!(
        states,
        [
            ("a missing", &BlueprintDefinitionState::Missing),
            ("b retired", &BlueprintDefinitionState::Retired),
        ]
    );
}

#[test]
fn blueprint_never_overrides_policy_or_decision() {
    let fx = Fixture::new("blueprint-evidence");
    let bp = fx
        .project
        .insert_blueprint(&NewBlueprint {
            blueprint_key: Some("tests".to_owned()),
            ..blueprint(KnowledgeScope::project(), "tests")
        })
        .expect("blueprint");
    fx.project
        .insert_blueprint_application(&NewBlueprintApplication {
            scope: fx.ws(),
            ..application(BlueprintOwnerKind::Project, bp.uid, "skip tests")
        })
        .expect("application");
    fx.project
        .insert_policy(&policy(
            KnowledgeScope::project(),
            Some("tests"),
            "run tests",
        ))
        .expect("policy");
    fx.project
        .insert_decision(&decision(KnowledgeScope::project(), "tests", "always"))
        .expect("decision");
    let result = fx.resolve(&ResolveRequest {
        decision_topics: vec!["tests".to_owned()],
        include_blueprints: true,
        ..ResolveRequest::new(fx.base())
    });
    assert_eq!(titles(&result.applied_policies), ["run tests"]);
    assert_eq!(chosen(&result.active_decisions), ["always"]);
    assert_eq!(result.blueprint_evidence.len(), 1);
    assert_eq!(result.blueprint_evidence[0].layer, 2);
    assert!(result.shadowed.is_empty());
    assert!(result.conflicts.is_empty());
}

// ----------------------------------------------------------- state (29-32)

#[test]
fn project_state_is_evidence_not_a_rule() {
    let fx = Fixture::new("state-evidence");
    fx.project
        .upsert_project_state(&state("tests", KnowledgeScope::project(), "failing"))
        .expect("state");
    fx.project
        .upsert_project_state(&state("unrequested", KnowledgeScope::project(), "x"))
        .expect("state");
    fx.project
        .upsert_project_state(&ProjectStateUpdate {
            status: ProjectStateStatus::Retired,
            ..state("retired", KnowledgeScope::project(), "x")
        })
        .expect("state");
    fx.project
        .insert_policy(&policy(
            KnowledgeScope::project(),
            Some("tests"),
            "run tests",
        ))
        .expect("policy");
    assert!(
        fx.resolve_base().state_evidence.is_empty(),
        "only on request"
    );
    let result = fx.resolve(&ResolveRequest {
        state_keys: vec!["tests".to_owned(), "retired".to_owned()],
        ..ResolveRequest::new(fx.base())
    });
    assert_eq!(titles(&result.applied_policies), ["run tests"]);
    assert_eq!(result.state_evidence.len(), 1);
    let evidence = &result.state_evidence[0];
    assert_eq!(evidence.item.key, "tests");
    assert_eq!(evidence.origin, Origin::Project);
    assert_eq!(evidence.reason, ResolutionReason::StateEvidence);
    assert!(result.shadowed.is_empty());
}

#[test]
fn workspace_project_state_stays_workspace_local() {
    let fx = Fixture::new("state-workspace");
    let other = WorkspaceKnowledgeStore::open(&fx.dir.path().join("other.db")).expect("other");
    other
        .upsert_workspace_project_state(&state("migration", KnowledgeScope::project(), "other"))
        .expect("state");
    fx.workspace
        .upsert_workspace_project_state(&state("migration", KnowledgeScope::project(), "mine"))
        .expect("state");
    let request = ResolveRequest {
        state_keys: vec!["migration".to_owned()],
        ..ResolveRequest::new(fx.base())
    };
    let result = fx.resolve(&request);
    assert_eq!(result.state_evidence.len(), 1);
    assert_eq!(result.state_evidence[0].origin, Origin::Workspace);
    assert_eq!(
        result.state_evidence[0].item.value,
        TypedValue::Text("mine".into())
    );
    let without = resolve(
        &KnowledgeSources {
            workspace: None,
            ..fx.sources()
        },
        &request,
    )
    .expect("resolve");
    assert!(without.state_evidence.is_empty());
}

fn new_item(store: &WorkspaceKnowledgeStore, goal: &str) -> WorkItem {
    let item = store
        .create_work_item(&NewWorkItem {
            source_kind: WorkItemSourceKind::Issue,
            source_ref: Some("#20".to_owned()),
            title: None,
            goal: goal.to_owned(),
        })
        .expect("item");
    store
        .set_work_item_status(item.uid, WorkItemStatus::Active)
        .expect("activate")
}

fn snapshot(item: WorkItemId, step: &str) -> WorkingState {
    WorkingState {
        work_item: item,
        baseline_workspace_revision: "rev-1".to_owned(),
        baseline_generation_no: 1,
        baseline_head: None,
        baseline_dirty_fingerprint: None,
        current_step: Some(step.to_owned()),
        progress_summary: None,
        remaining_summary: None,
        blocker_summary: None,
        owner_agent: Some("agent".to_owned()),
        last_observed_workspace_revision: "rev-1".to_owned(),
        updated_at: String::new(),
    }
}

#[test]
fn supplied_work_item_contributes_working_state_evidence() {
    let fx = Fixture::new("work-item");
    let item = new_item(&fx.workspace, "resolver");
    fx.workspace
        .upsert_working_state(&snapshot(item.uid, "writing tests"))
        .expect("state");
    let result = fx.resolve(&ResolveRequest {
        work_item: Some(item.uid),
        ..ResolveRequest::new(fx.base())
    });
    let Some(WorkItemEvidence::Found {
        item: found,
        working_state: Some(state),
    }) = result.working_state
    else {
        panic!("supplied WorkItem must be evidence");
    };
    assert_eq!(found.uid, item.uid);
    assert_eq!(state.current_step.as_deref(), Some("writing tests"));

    let unknown = WorkItemId::generate();
    let result = fx.resolve(&ResolveRequest {
        work_item: Some(unknown),
        ..ResolveRequest::new(fx.base())
    });
    assert_eq!(
        result.working_state,
        Some(WorkItemEvidence::Missing(unknown))
    );
}

#[test]
fn no_work_item_supplied_means_none_is_guessed() {
    let fx = Fixture::new("work-item-none");
    let item = new_item(&fx.workspace, "only active item");
    fx.workspace
        .upsert_working_state(&snapshot(item.uid, "step"))
        .expect("state");
    assert_eq!(fx.resolve_base().working_state, None);
    let request = ResolveRequest {
        work_item: Some(item.uid),
        ..ResolveRequest::new(fx.base())
    };
    let no_workspace = KnowledgeSources {
        workspace: None,
        ..fx.sources()
    };
    assert!(matches!(
        resolve(&no_workspace, &request),
        Err(ResolveError::InvalidRequest(_))
    ));
}

// ---------------------------------------------- incomparable / order (33-34)

#[test]
fn same_layer_incomparable_scopes_get_no_arbitrary_winner() {
    let fx = Fixture::new("incomparable");
    fx.project
        .insert_policy(&policy(domain("database"), Some("review"), "db review"))
        .expect("policy");
    fx.project
        .insert_policy(&policy(
            domain("security"),
            Some("review"),
            "security review",
        ))
        .expect("policy");
    fx.project
        .insert_decision(&decision(domain("database"), "owner", "dba team"))
        .expect("decision");
    fx.project
        .insert_decision(&decision(domain("security"), "owner", "security team"))
        .expect("decision");
    let context = fx
        .base()
        .with_layer(vec![domain("security"), domain("database")])
        .expect("ctx");
    let result = fx.resolve(&ResolveRequest {
        decision_topics: vec!["owner".to_owned()],
        ..ResolveRequest::new(context)
    });
    assert_eq!(
        titles(&result.applied_policies),
        ["db review", "security review"]
    );
    assert!(result.active_decisions.is_empty());
    assert_eq!(
        conflict_kinds(&result),
        [ConflictKind::SameSpecificityDecision]
    );
    let scopes: BTreeSet<_> = result.conflicts[0]
        .involved
        .iter()
        .map(|e| e.scope.clone())
        .collect();
    assert_eq!(
        scopes,
        BTreeSet::from([domain("database"), domain("security")])
    );
}

/// Stable, uid-free view of a result, so two stores holding the same
/// logical knowledge under different uids can be compared exactly.
fn logical(result: &ResolvedKnowledge) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |section: &str, entry: String| out.push(format!("{section}: {entry}"));
    for e in &result.request_directives {
        push(
            "directive",
            format!("{} {:?} {}", e.item.id, e.reason, e.layer),
        );
    }
    for e in &result.protected_constraints {
        push(
            "protected",
            format!("{} {:?} {:?}", e.item.title, e.origin, e.reason),
        );
    }
    for e in &result.applied_policies {
        push(
            "policy",
            format!("{} {:?} {} {:?}", e.item.title, e.origin, e.layer, e.reason),
        );
    }
    for e in &result.active_decisions {
        push(
            "decision",
            format!("{} {} {:?}", e.item.topic, e.item.chosen_summary, e.layer),
        );
    }
    for e in &result.applied_preferences {
        push(
            "preference",
            format!("{} {}", e.item.preference_key, e.item.value.to_json()),
        );
    }
    for e in &result.shadowed {
        push(
            "shadowed",
            format!("{:?} {:?} {}", e.reason, e.origin, e.layer),
        );
    }
    for c in &result.conflicts {
        push(
            "conflict",
            format!("{:?} {:?} {}", c.kind, c.subject, c.involved.len()),
        );
    }
    out
}

fn populate(fx: &Fixture, order: &[usize]) {
    for &step in order {
        match step {
            0 => {
                fx.project
                    .insert_policy(&policy(fx.ws(), Some("tests"), "keyed ws"))
                    .expect("seed");
            }
            1 => {
                fx.project
                    .insert_policy(&policy(
                        KnowledgeScope::project(),
                        Some("tests"),
                        "keyed project",
                    ))
                    .expect("seed");
            }
            2 => {
                fx.project
                    .insert_policy(&policy(KnowledgeScope::project(), None, "unkeyed a"))
                    .expect("seed");
            }
            3 => {
                fx.project
                    .insert_policy(&policy(KnowledgeScope::project(), None, "unkeyed b"))
                    .expect("seed");
            }
            4 => {
                fx.project
                    .insert_decision(&decision(KnowledgeScope::project(), "db", "sqlite"))
                    .expect("seed");
            }
            5 => {
                fx.project
                    .insert_decision(&decision(KnowledgeScope::project(), "db", "postgres"))
                    .expect("seed");
            }
            6 => {
                fx.project
                    .insert_decision(&decision(fx.ws(), "lang", "rust"))
                    .expect("seed");
            }
            7 => {
                fx.global
                    .insert_user_preference(&preference(
                        KnowledgeScope::global(),
                        "editor",
                        "helix",
                    ))
                    .expect("seed");
            }
            8 => {
                fx.global
                    .insert_user_preference(&preference(KnowledgeScope::global(), "lang", "python"))
                    .expect("seed");
            }
            9 => {
                fx.global
                    .insert_user_policy(&protected(
                        KnowledgeScope::global(),
                        "secrets",
                        "protected",
                        ProtectionClass::ProtectedPrivacy,
                        SourceKind::UserExplicit,
                    ))
                    .expect("seed");
            }
            10 => {
                fx.global
                    .insert_user_policy(&policy(
                        KnowledgeScope::global(),
                        Some("tests"),
                        "keyed global",
                    ))
                    .expect("seed");
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn result_is_deterministic_across_insertion_order() {
    let orders: [&[usize]; 4] = [
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
        &[10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
        &[5, 3, 9, 1, 7, 0, 10, 2, 8, 4, 6],
        &[2, 4, 6, 8, 10, 0, 1, 3, 5, 7, 9],
    ];
    let mut views = Vec::new();
    for (index, order) in orders.iter().enumerate() {
        let fx = Fixture::new(&format!("determinism-{index}"));
        let fixed = Fixture {
            workspace_id: WorkspaceId::from_bytes([7; 16]),
            ..fx
        };
        populate(&fixed, order);
        let request = ResolveRequest {
            directives: vec![directive(
                "d1",
                DirectiveTarget::Policy,
                "secrets",
                KnowledgeScope::project(),
            )],
            decision_topics: vec!["db".to_owned(), "lang".to_owned()],
            preference_keys: vec!["lang".to_owned(), "editor".to_owned()],
            ..ResolveRequest::new(fixed.base())
        };
        let first = fixed.resolve(&request);
        assert_eq!(fixed.resolve(&request), first, "same store, same answer");
        views.push(logical(&first));
    }
    assert!(
        views.windows(2).all(|pair| pair[0] == pair[1]),
        "{views:#?}"
    );
    assert_eq!(
        views[0],
        [
            "protected: protected Global ProtectedConstraint",
            "policy: unkeyed a Project 1 UnkeyedPolicy",
            "policy: unkeyed b Project 1 UnkeyedPolicy",
            "policy: keyed ws Project 2 SelectedPolicy",
            "decision: lang rust 2",
            "preference: editor \"helix\"",
            "shadowed: ShadowedByProjectDecision Global 0",
            "shadowed: ShadowedByProjectTier Global 0",
            "shadowed: ShadowedByMoreSpecificScope Project 1",
            "conflict: SameSpecificityDecision Some(\"db\") 2",
            "conflict: ProtectedOverrideRejected Some(\"secrets\") 2",
        ]
    );
}

// ----------------------------------------------- isolation / privacy (37-38)

#[test]
fn worktree_scoped_knowledge_does_not_leak_across_workspaces() {
    let home = TestDir::create("wt-home");
    let main = TestDir::create("wt-main");
    let secondary = TestDir::create("wt-secondary");
    fs::remove_dir_all(secondary.path()).expect("worktree target must not exist yet");
    run_git(&["init", "-q"], main.path());
    run_git(
        &["commit", "-q", "--allow-empty", "-m", "init"],
        main.path(),
    );
    run_git(
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            &secondary.path().to_string_lossy(),
        ],
        main.path(),
    );
    let global_paths = GlobalPaths::from_home(home.path());
    let a = init_workspace(main.path(), &global_paths).expect("main init");
    let b = init_workspace(secondary.path(), &global_paths).expect("secondary init");
    assert_eq!(a.project_id, b.project_id);
    assert_ne!(a.workspace_id, b.workspace_id);

    let registry = GlobalRegistry::open(&global_paths.global_db).expect("registry");
    let project =
        ProjectKnowledgeStore::open_project_home(&registry, a.project_id).expect("project");
    let global = GlobalKnowledgeStore::open(&global_paths.global_db).expect("global");
    let ws_a =
        WorkspaceKnowledgeStore::open(&WorkspacePaths::from_root(&a.workspace_root).workspace_db)
            .expect("ws a");
    let ws_b =
        WorkspaceKnowledgeStore::open(&WorkspacePaths::from_root(&b.workspace_root).workspace_db)
            .expect("ws b");

    project
        .insert_policy(&policy(KnowledgeScope::project(), Some("tests"), "shared"))
        .expect("policy");
    project
        .insert_policy(&policy(
            KnowledgeScope::workspace(a.workspace_id),
            Some("tests"),
            "only A",
        ))
        .expect("policy");
    project
        .insert_decision(&decision(
            KnowledgeScope::workspace(a.workspace_id),
            "branch",
            "A choice",
        ))
        .expect("decision");
    ws_a.upsert_workspace_project_state(&state("migration", KnowledgeScope::project(), "A"))
        .expect("state");
    let item_a = new_item(&ws_a, "A work");

    let request = |workspace: WorkspaceId, item: Option<WorkItemId>| ResolveRequest {
        decision_topics: vec!["branch".to_owned()],
        state_keys: vec!["migration".to_owned()],
        work_item: item,
        ..ResolveRequest::new(ApplicabilityContext::base(Some(workspace)))
    };
    let sources = |workspace| KnowledgeSources {
        global: &global,
        project: &project,
        workspace: Some(workspace),
    };

    let in_a = resolve(&sources(&ws_a), &request(a.workspace_id, Some(item_a.uid))).expect("a");
    assert_eq!(titles(&in_a.applied_policies), ["only A"]);
    assert_eq!(chosen(&in_a.active_decisions), ["A choice"]);
    assert_eq!(in_a.state_evidence.len(), 1);
    assert!(matches!(
        in_a.working_state,
        Some(WorkItemEvidence::Found { .. })
    ));

    let in_b = resolve(&sources(&ws_b), &request(b.workspace_id, Some(item_a.uid))).expect("b");
    assert_eq!(titles(&in_b.applied_policies), ["shared"]);
    assert!(in_b.active_decisions.is_empty());
    assert!(in_b.state_evidence.is_empty());
    assert_eq!(
        in_b.working_state,
        Some(WorkItemEvidence::Missing(item_a.uid))
    );

    // A's workspace.db cannot answer for B's context.
    assert!(matches!(
        resolve(&sources(&ws_a), &request(b.workspace_id, None)),
        Err(ResolveError::InvalidRequest(_))
    ));
}

#[test]
fn global_storage_is_not_injected_by_default() {
    let fx = Fixture::new("global-privacy");
    fx.global
        .insert_user_preference(&preference(KnowledgeScope::global(), "diet", "vegan"))
        .expect("preference");
    fx.global
        .insert_user_preference(&preference(domain("health"), "doctor", "dr x"))
        .expect("preference");
    fx.global
        .insert_user_policy(&policy(domain("finance"), Some("budget"), "finance rule"))
        .expect("policy");
    fx.global
        .insert_blueprint(&blueprint(KnowledgeScope::global(), "unrelated reusable"))
        .expect("blueprint");
    fx.global
        .insert_blueprint(&blueprint(domain("finance"), "finance reusable"))
        .expect("blueprint");
    let context = fx.base().with_layer(vec![domain("database")]).expect("ctx");
    let result = fx.resolve(&ResolveRequest {
        preference_keys: vec!["language".to_owned()],
        include_blueprints: true,
        ..ResolveRequest::new(context)
    });
    assert_eq!(result, ResolvedKnowledge::default());
}

// ------------------------------------------------- bounds / purity (35-36)

#[test]
fn bound_overflow_is_an_error_not_a_truncation() {
    let fx = Fixture::new("bound");
    for index in 0..=FETCH_BOUND {
        fx.project
            .insert_policy(&policy(
                KnowledgeScope::project(),
                None,
                &format!("p{index}"),
            ))
            .expect("policy");
    }
    assert!(matches!(
        resolve(&fx.sources(), &ResolveRequest::new(fx.base())),
        Err(ResolveError::BoundExceeded { .. })
    ));
}

#[test]
fn resolver_is_read_only() {
    let fx = Fixture::new("read-only");
    populate(&fx, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    let item = new_item(&fx.workspace, "w");
    let dump = |fx: &Fixture| -> Vec<String> {
        let raw = fx.raw_project();
        [
            "policy",
            "decision",
            "blueprint_application",
            "project_state",
        ]
        .iter()
        .map(|table| {
            raw.query_row(
                &format!("SELECT count(*) || ':' || ifnull(max(updated_at), '') FROM {table}"),
                [],
                |row| row.get(0),
            )
            .expect("dump")
        })
        .collect()
    };
    let before = dump(&fx);
    fx.resolve(&ResolveRequest {
        directives: vec![directive(
            "d",
            DirectiveTarget::Decision,
            "db",
            KnowledgeScope::project(),
        )],
        decision_topics: vec!["db".to_owned()],
        preference_keys: vec!["lang".to_owned()],
        state_keys: vec!["x".to_owned()],
        include_blueprints: true,
        work_item: Some(item.uid),
        ..ResolveRequest::new(fx.base())
    });
    assert_eq!(dump(&fx), before);
}

const RESOLVER_SOURCE: &str = include_str!("../resolve.rs");

#[test]
fn resolver_starts_no_semantic_backend() {
    let imports: Vec<_> = RESOLVER_SOURCE
        .lines()
        .filter(|line| line.starts_with("use "))
        .collect();
    assert_eq!(
        imports,
        [
            "use std::{",
            "use brainprint_core::{WorkItemId, WorkspaceId};",
            "use super::{"
        ],
        "the resolver reads only the knowledge stores"
    );
    for forbidden in ["semantic", "lsp", "Command", "spawn", "index.db", "crate::"] {
        assert!(!RESOLVER_SOURCE.contains(forbidden), "{forbidden}");
    }
}

#[test]
fn resolver_has_no_model_or_similarity_call() {
    let lower = RESOLVER_SOURCE.to_lowercase();
    for forbidden in [
        "llm",
        "embedding",
        "similar",
        "fuzzy",
        "http",
        "prompt",
        "contains(\"",
        "updated_at",
        "created_at",
    ] {
        assert!(!lower.contains(forbidden), "{forbidden}");
    }
}
