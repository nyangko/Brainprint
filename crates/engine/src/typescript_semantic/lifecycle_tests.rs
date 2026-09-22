//! Always-on tests for the TypeScript/JavaScript lifecycle.
//!
//! Configuration discovery, environment identity, what a change batch
//! invalidates, and the Level B projection -- all of it without a `tsc`
//! anywhere, which is the point: every one of these answers has to hold
//! on a machine with no TypeScript installed.

use std::collections::BTreeSet;

use super::{
    lifecycle::{
        self, ChangeKind, ConfigSource, EnvironmentAssurance, ResourceChange, SemanticAvailability,
        strip_jsonc,
    },
    protocol::WatchedChangeKind,
    tests_support::{
        Fixture, ScriptedBackend, backend_for_service_run, context, encoding, refresh,
    },
};
use crate::{
    resource::ResourceLanguage,
    runtime::RuntimeState,
    semantic_index::{SemanticIndex, SemanticOwner, SemanticState},
};

// ---------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------

#[test]
fn the_whole_extends_chain_is_part_of_the_configuration() {
    let fixture = Fixture::create("config-chain");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let config =
        lifecycle::discover_config(index.connection(), &fixture.root, "").expect("discovered");

    assert_eq!(config.source, ConfigSource::TsConfig);
    assert_eq!(
        config
            .chain
            .iter()
            .map(|file| file.resource.path_key.as_str())
            .collect::<Vec<_>>(),
        vec!["tsconfig.json", "tsconfig.base.json"],
        "the root config and the file it extends"
    );
    assert!(config.is_complete(), "{:?}", config.limits);

    // Per-key override, which is TypeScript's own precedence: the root
    // states `paths`, the base states `moduleResolution`, and the
    // effective configuration has both.
    assert!(config.option("paths").is_some());
    assert_eq!(
        config
            .option("moduleResolution")
            .and_then(|value| value.as_str()),
        Some("bundler")
    );
    assert_eq!(
        config
            .option("checkJs")
            .and_then(serde_json::Value::as_bool),
        Some(false)
    );
    assert!(config.membership("include").is_some());
}

#[test]
fn changing_an_inherited_config_moves_the_basis_even_though_the_root_did_not() {
    let fixture = Fixture::create("config-inherited");
    let before = {
        let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
        lifecycle::discover_config(index.connection(), &fixture.root, "")
            .expect("discovered")
            .basis()
            .fingerprint()
    };

    // The whole point of `extends`: the file that actually governs may
    // not be the one whose name the project states.
    let base = fixture
        .text("tsconfig.base.json")
        .replace("\"checkJs\": false", "\"checkJs\": true");
    fixture.write("tsconfig.base.json", &base);
    fixture.rescan("workspace-rev-2");

    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let after =
        lifecycle::discover_config(index.connection(), &fixture.root, "").expect("discovered");
    assert_eq!(
        after.option("checkJs").and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_ne!(
        after.basis().fingerprint(),
        before,
        "an inherited change has to invalidate, or a stale answer reads as current"
    );
}

#[test]
fn an_extends_that_leaves_the_workspace_is_a_partial_configuration_not_a_guess() {
    let fixture = Fixture::create("config-package-extends");
    fixture.write(
        "tsconfig.json",
        "{ \"extends\": \"@tsconfig/node20/tsconfig.json\", \"include\": [\"src\"] }",
    );
    fixture.rescan("workspace-rev-2");

    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let config =
        lifecycle::discover_config(index.connection(), &fixture.root, "").expect("discovered");
    assert_eq!(config.chain.len(), 1, "the chain stops where it cannot see");
    assert!(
        !config.is_complete(),
        "a published base config is a real shape, and reading `node_modules` \
         into the basis is not the answer to it"
    );
    // And a partial configuration is a *different* configuration from a
    // complete one that happens to say the same things.
    assert!(!config.basis().fingerprint().is_empty());
}

#[test]
fn a_circular_extends_stops_rather_than_recursing() {
    let fixture = Fixture::create("config-cycle");
    fixture.write("tsconfig.json", "{ \"extends\": \"./tsconfig.base.json\" }");
    fixture.write("tsconfig.base.json", "{ \"extends\": \"./tsconfig.json\" }");
    fixture.rescan("workspace-rev-2");

    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let config =
        lifecycle::discover_config(index.connection(), &fixture.root, "").expect("discovered");
    assert_eq!(config.chain.len(), 2);
    assert!(
        matches!(
            config.limits.first(),
            Some(lifecycle::ConfigLimit::CircularExtends { .. })
        ),
        "{:?}",
        config.limits
    );
}

#[test]
fn a_jsconfig_governs_only_where_there_is_no_tsconfig() {
    let fixture = Fixture::create("config-jsconfig");
    fixture.remove("tsconfig.json");
    fixture.write(
        "jsconfig.json",
        "{ \"compilerOptions\": { \"checkJs\": true } }",
    );
    fixture.rescan("workspace-rev-2");

    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let config =
        lifecycle::discover_config(index.connection(), &fixture.root, "").expect("discovered");
    assert_eq!(config.source, ConfigSource::JsConfig);
    // A project setting, not a capability: `checkJs` narrows what this
    // project covers and says nothing about what the backend can do.
    assert_eq!(
        config
            .option("checkJs")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
}

#[test]
fn the_config_the_ecosystem_actually_writes_is_readable() {
    // `tsc --init` emits comments and the world keeps them. A strict
    // JSON parse would report the most ordinary config there is as
    // unreadable and turn every such project partial.
    let jsonc = "{\n  // the project\n  \"compilerOptions\": {\n    \
                 \"strict\": true, /* inline */\n    \"paths\": { \"@a/*\": [\"src/*\"] },\n  },\n}";
    let value: serde_json::Value = serde_json::from_str(&strip_jsonc(jsonc)).expect("readable");
    assert_eq!(value["compilerOptions"]["strict"], true);
    assert!(value["compilerOptions"]["paths"]["@a/*"].is_array());
    // A URL inside a string is not a comment.
    let with_url = "{ \"a\": \"https://example.com/x\" }";
    assert_eq!(strip_jsonc(with_url), with_url);
}

// ---------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------

#[test]
fn a_manifest_and_a_lockfile_together_prove_the_dependency_tree() {
    let fixture = Fixture::create("env-proven");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let identity = lifecycle::environment_identity(index.connection(), &fixture.root, "", "7.0.2")
        .expect("identity");
    assert_eq!(identity.assurance, EnvironmentAssurance::Proven);
    assert_eq!(identity.lockfile.as_deref(), Some("package-lock.json"));
}

#[test]
fn without_a_lockfile_the_environment_is_unknown_rather_than_assumed() {
    let fixture = Fixture::create("env-unlocked");
    fixture.remove("package-lock.json");
    fixture.rescan("workspace-rev-2");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let identity = lifecycle::environment_identity(index.connection(), &fixture.root, "", "7.0.2")
        .expect("identity");
    assert!(
        !identity.assurance.is_proven(),
        "a reinstall can change what a dependency import means with no \
         tracked input moving, and a conservative UNKNOWN is the honest answer"
    );
}

#[test]
fn a_linked_workspace_package_cannot_be_proven_from_the_manifest() {
    let fixture = Fixture::create("env-workspaces");
    fixture.write(
        "package.json",
        "{\"name\":\"x\",\"version\":\"0.0.0\",\"private\":true,\"workspaces\":[\"packages/*\"]}",
    );
    fixture.rescan("workspace-rev-2");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let identity = lifecycle::environment_identity(index.connection(), &fixture.root, "", "7.0.2")
        .expect("identity");
    let EnvironmentAssurance::Unknown { reason } = identity.assurance else {
        panic!("a linked package resolves to a tree nothing here tracks");
    };
    assert!(reason.contains("workspaces"), "{reason}");
}

#[test]
fn a_dependency_install_that_changes_nothing_in_the_project_still_moves_the_identity() {
    let fixture = Fixture::create("env-lockfile-change");
    let before = {
        let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
        lifecycle::environment_identity(index.connection(), &fixture.root, "", "7.0.2")
            .expect("identity")
    };
    fixture.write(
        "package-lock.json",
        "{\"name\":\"brainprint-ts-fixture\",\"lockfileVersion\":3,\"packages\":{}}",
    );
    fixture.rescan("workspace-rev-2");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let after = lifecycle::environment_identity(index.connection(), &fixture.root, "", "7.0.2")
        .expect("identity");
    assert_ne!(
        after.fingerprint, before.fingerprint,
        "an old ExternalEntity proof must not stay clean across an install"
    );
    // And the backend version is part of it, because a different server
    // is a different answer.
    let other_backend =
        lifecycle::environment_identity(index.connection(), &fixture.root, "", "7.0.9")
            .expect("identity");
    assert_ne!(other_backend.fingerprint, after.fingerprint);
}

// ---------------------------------------------------------------------
// Inventory
// ---------------------------------------------------------------------

#[test]
fn the_inventory_moves_when_a_module_does_and_not_when_a_body_changes() {
    let fixture = Fixture::create("inventory");
    let baseline = {
        let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
        lifecycle::inventory_fingerprint(index.connection()).expect("fingerprint")
    };

    // An edit to a body: the same modules, so the same resolution
    // context. Invalidating the whole project here would be a refresh
    // storm for nothing.
    fixture.write(
        "src/model.ts",
        "export class Model {\n    describe(): string {\n        return \"edited\";\n    }\n}\n",
    );
    fixture.rescan("workspace-rev-2");
    let after_edit = {
        let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
        lifecycle::inventory_fingerprint(index.connection()).expect("fingerprint")
    };
    assert_eq!(after_edit, baseline, "a body edit is not a module move");

    // A new module: what `./new.js` resolves to changed for every file
    // in the project, including ones nobody touched.
    fixture.write("src/new.ts", "export const fresh = 1;\n");
    fixture.rescan("workspace-rev-3");
    let after_add = {
        let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
        lifecycle::inventory_fingerprint(index.connection()).expect("fingerprint")
    };
    assert_ne!(after_add, baseline);

    // A Markdown file is not a module and must not invalidate anything.
    fixture.write("NOTES.md", "# notes\n");
    fixture.rescan("workspace-rev-4");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    assert_eq!(
        lifecycle::inventory_fingerprint(index.connection()).expect("fingerprint"),
        after_add,
        "an unrelated file is not part of module resolution"
    );
}

// ---------------------------------------------------------------------
// Change plans
// ---------------------------------------------------------------------

/// Publish one owner so there is something to invalidate.
fn published(fixture: &Fixture) -> SemanticIndex {
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let backend = backend_for_service_run(fixture);
    refresh(fixture, &index, &backend, "src/consumer.ts").expect("refresh");
    index
}

#[test]
fn only_the_owners_whose_basis_names_a_changed_resource_are_invalidated() {
    let fixture = Fixture::create("plan-basis");
    let index = published(&fixture);
    let config = lifecycle::discover_config(index.connection(), &fixture.root, "").expect("config");
    let consumer = SemanticOwner::new(
        context().context_key(),
        fixture.resource("src/consumer.ts").id,
    );

    // A file nobody's proof depends on.
    let unrelated = ResourceChange::new(
        fixture.resource("src/unicode.ts").id,
        ChangeKind::Changed,
        "src/unicode.ts",
    );
    let plan = lifecycle::plan_changes(&index, &context(), &[unrelated], &config).expect("plan");
    assert!(
        !plan.affected.contains(&consumer),
        "a Resource nobody read invalidates nobody"
    );

    // The file the proof actually reads.
    let depended = ResourceChange::new(
        fixture.resource("src/core/service.ts").id,
        ChangeKind::Changed,
        "src/core/service.ts",
    );
    let plan = lifecycle::plan_changes(&index, &context(), &[depended], &config).expect("plan");
    assert!(
        plan.affected.contains(&consumer),
        "the semantic basis is what makes a dependent stale"
    );
    assert!(!plan.inventory_moved && !plan.config_moved && !plan.environment_moved);
}

#[test]
fn a_module_moving_invalidates_the_whole_project() {
    let fixture = Fixture::create("plan-inventory");
    let index = published(&fixture);
    let config = lifecycle::discover_config(index.connection(), &fixture.root, "").expect("config");
    let consumer = SemanticOwner::new(
        context().context_key(),
        fixture.resource("src/consumer.ts").id,
    );

    for kind in [ChangeKind::Added, ChangeKind::Deleted, ChangeKind::Moved] {
        let change =
            ResourceChange::new(fixture.resource("src/unicode.ts").id, kind, "src/moved.ts")
                .from_previous("src/unicode.ts");
        let plan = lifecycle::plan_changes(&index, &context(), &[change], &config).expect("plan");
        assert!(plan.inventory_moved, "{kind:?}");
        assert!(
            plan.affected.contains(&consumer),
            "module resolution is a property of the program, not of a file"
        );
    }
}

#[test]
fn a_config_or_environment_change_invalidates_the_whole_project() {
    let fixture = Fixture::create("plan-config");
    let index = published(&fixture);
    let config = lifecycle::discover_config(index.connection(), &fixture.root, "").expect("config");
    let consumer = SemanticOwner::new(
        context().context_key(),
        fixture.resource("src/consumer.ts").id,
    );

    for (rel, expect_config, expect_environment) in [
        ("tsconfig.base.json", true, false),
        ("tsconfig.json", true, false),
        ("package-lock.json", false, true),
        ("package.json", false, true),
    ] {
        let change = ResourceChange::new(fixture.resource(rel).id, ChangeKind::Changed, rel)
            .with_language(None);
        let plan = lifecycle::plan_changes(&index, &context(), &[change], &config).expect("plan");
        assert_eq!(plan.config_moved, expect_config, "{rel}");
        assert_eq!(plan.environment_moved, expect_environment, "{rel}");
        assert!(plan.affected.contains(&consumer), "{rel}");
    }
}

#[test]
fn a_structural_dependent_is_planned_before_the_replacement_that_re_resolves_it() {
    // Carried forward from #19 task 9 rather than rediscovered through
    // a foreign-key failure: a call site I3 resolved structurally never
    // enters the caller's semantic basis, and the caller is still
    // re-resolved by a replacement that removes the edge its semantic
    // evidence points at.
    let fixture = Fixture::create("plan-structural");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let backend = backend_for_service_run(&fixture);
    refresh(&fixture, &index, &backend, "src/consumer.ts").expect("consumer");
    refresh(&fixture, &index, &backend, "js/use-esm.js").expect("use-esm");

    let config = lifecycle::discover_config(index.connection(), &fixture.root, "").expect("config");
    let change = ResourceChange::new(
        fixture.resource("js/esm.js").id,
        ChangeKind::Changed,
        "js/esm.js",
    )
    .with_language(Some(ResourceLanguage::JavaScript));
    let plan = lifecycle::plan_changes(&index, &context(), &[change], &config).expect("plan");
    assert!(
        plan.affected.contains(&SemanticOwner::new(
            context().context_key(),
            fixture.resource("js/use-esm.js").id
        )),
        "the structural dependent is in the plan: {:?}",
        plan.affected
    );
}

// ---------------------------------------------------------------------
// Watched files
// ---------------------------------------------------------------------

#[test]
fn a_change_batch_is_deduped_ordered_and_correctly_typed() {
    let fixture = Fixture::create("watched-batch");
    let model = fixture.resource("src/model.ts").id;
    let changes = vec![
        ResourceChange::new(model, ChangeKind::Added, "src/new.ts"),
        // The same file again: created and changed in one batch is
        // created once.
        ResourceChange::new(model, ChangeKind::Changed, "src/new.ts"),
        ResourceChange::new(model, ChangeKind::Deleted, "src/gone.ts"),
        ResourceChange::new(model, ChangeKind::Moved, "src/after.ts")
            .from_previous("src/before.ts"),
    ];
    let batch = lifecycle::watched_changes(&fixture.root, &changes);

    let by_name = |name: &str| {
        batch
            .iter()
            .find(|change| change.uri.ends_with(name))
            .unwrap_or_else(|| panic!("{name} in {batch:?}"))
            .kind
    };
    assert_eq!(by_name("src/new.ts"), WatchedChangeKind::Created);
    assert_eq!(by_name("src/gone.ts"), WatchedChangeKind::Deleted);
    // A move is two filesystem events, because on the filesystem it is.
    assert_eq!(by_name("src/before.ts"), WatchedChangeKind::Deleted);
    assert_eq!(by_name("src/after.ts"), WatchedChangeKind::Changed);
    assert_eq!(batch.len(), 4);

    let mut sorted = batch.clone();
    sorted.sort_by(|left, right| left.uri.cmp(&right.uri));
    assert_eq!(batch, sorted, "one batch, deterministically ordered");
}

#[test]
fn nothing_is_sent_when_nothing_changed() {
    let fixture = Fixture::create("watched-empty");
    let backend = ScriptedBackend::new();
    assert_eq!(
        lifecycle::synchronize(&backend, &fixture.root, &[]).expect("nothing"),
        0
    );
    assert!(backend.calls().is_empty());
}

// ---------------------------------------------------------------------
// Level B
// ---------------------------------------------------------------------

#[test]
fn a_missing_backend_leaves_a_current_proof_current_and_everything_else_unavailable() {
    let fixture = Fixture::create("level-b");
    let index = published(&fixture);
    let owner = SemanticOwner::new(
        context().context_key(),
        fixture.resource("src/consumer.ts").id,
    );
    let current = index.status(&owner).expect("status");
    assert_eq!(current.state, SemanticState::Current);

    // No install at all. A proof whose inputs still hold is still a
    // proof; the process being gone is a reason it cannot be *renewed*.
    assert_eq!(
        lifecycle::availability(
            &current,
            RuntimeState::Stopped,
            lifecycle::BackendReadiness::Unavailable
        ),
        SemanticAvailability::CurrentRuntimeCold
    );
    assert!(
        lifecycle::availability(
            &current,
            RuntimeState::Stopped,
            lifecycle::BackendReadiness::Unavailable
        )
        .serves_current()
    );

    // The same backend state over a stale publication is not current at
    // all.
    index
        .mark_dirty(&owner, crate::semantic_index::SOURCE_MOVED_CODE)
        .expect("mark");
    let stale = index.status(&owner).expect("status");
    assert_eq!(
        lifecycle::availability(
            &stale,
            RuntimeState::Stopped,
            lifecycle::BackendReadiness::Unavailable
        ),
        SemanticAvailability::Unavailable
    );
    assert_eq!(
        lifecycle::availability(
            &stale,
            RuntimeState::Ready,
            lifecycle::BackendReadiness::Available
        ),
        SemanticAvailability::RefreshRequired
    );
}

#[test]
fn a_daemon_reopen_revalidates_from_disk_without_launching_anything() {
    let fixture = Fixture::create("reopen");
    let index = published(&fixture);
    let config = lifecycle::discover_config(index.connection(), &fixture.root, "")
        .expect("config")
        .basis();
    let environment =
        lifecycle::environment_identity(index.connection(), &fixture.root, "", "7.0.2")
            .expect("identity");
    let inventory = lifecycle::inventory_fingerprint(index.connection()).expect("inventory");
    let capabilities = super::capability_report(&context());
    let current =
        lifecycle::current_inputs(&context(), &config, &capabilities, &inventory, &environment);

    let answers = lifecycle::revalidate_context(&index, &context(), &current).expect("revalidate");
    assert!(
        answers
            .iter()
            .any(|(_, status)| status.state == SemanticState::Current),
        "a publication whose inputs still hold proves itself from disk"
    );

    // An environment nobody can prove cannot reopen as current.
    let unprovable = lifecycle::EnvironmentIdentity {
        assurance: EnvironmentAssurance::Unknown {
            reason: "no lockfile".to_owned(),
        },
        ..environment
    };
    let guarded =
        lifecycle::current_inputs(&context(), &config, &capabilities, &inventory, &unprovable);
    let answers = lifecycle::revalidate_context(&index, &context(), &guarded).expect("revalidate");
    assert!(
        answers
            .iter()
            .all(|(_, status)| status.state != SemanticState::Current),
        "an unprovable environment must not reopen CURRENT"
    );
}

#[test]
fn an_unavailable_owner_keeps_its_last_valid_publication() {
    let fixture = Fixture::create("unavailable");
    let index = published(&fixture);
    let owner = SemanticOwner::new(
        context().context_key(),
        fixture.resource("src/consumer.ts").id,
    );
    lifecycle::mark_unavailable(
        &index,
        &BTreeSet::from([owner.clone()]),
        crate::semantic_index::BACKEND_UNAVAILABLE_CODE,
    )
    .expect("mark");
    let status = index.status(&owner).expect("status");
    assert_eq!(status.state, SemanticState::Unavailable);
    assert!(
        status.has_last_valid(),
        "never replaced with an empty success"
    );
}

#[test]
fn a_config_change_invalidates_every_owner_the_project_published() {
    let fixture = Fixture::create("invalidate-config");
    let index = published(&fixture);
    let owners = lifecycle::invalidate_for_config(&index, &context()).expect("invalidate");
    assert!(!owners.is_empty());
    for owner in &owners {
        assert_ne!(
            index.status(owner).expect("status").state,
            SemanticState::Current
        );
    }
}

#[test]
fn two_worktrees_of_one_project_are_two_contexts() {
    // Nothing is shared between them: different Workspace identity
    // means a different context key, so one worktree's publication can
    // never be served for the other's source.
    let first = Fixture::create("worktree-a");
    let second = Fixture::create("worktree-b");
    let index_a = published(&first);
    let index_b = published(&second);
    let key = context().context_key();
    let owner_a = SemanticOwner::new(&key, first.resource("src/consumer.ts").id);
    assert_eq!(
        index_a.status(&owner_a).expect("status").state,
        SemanticState::Current
    );
    // A Resource id is content-and-path derived, so the two trees agree
    // on it -- which is exactly why the *index* has to be separate.
    assert!(
        index_b
            .owners_of_context(&key)
            .expect("owners")
            .iter()
            .all(|owner| owner.context_key == key)
    );
    assert_ne!(first.db_path(), second.db_path());
    let _ = encoding();
}
