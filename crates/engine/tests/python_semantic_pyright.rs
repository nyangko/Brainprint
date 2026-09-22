//! End-to-end Python semantics against the real pinned Pyright.
//!
//! Ignored by default and skipped when the pinned install is absent, so
//! the normal workspace suite never depends on anyone having Pyright --
//! globally or otherwise. To run it:
//!
//! ```sh
//! cd scripts/python_semantic_spike && npm install && cd -
//! cargo test -p brainprint-engine --test python_semantic_pyright -- --ignored --nocapture
//! ```
//!
//! What it proves that the scripted tests cannot: that the answers the
//! adapter is built around are the answers the real `pyright-typeserver`
//! 1.1.414 actually gives, over the real
//! `fixtures/workspaces/python-semantic-spike` package.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process,
    sync::Arc,
    time::{Duration, Instant},
};

use brainprint_core::WorkspaceId;
use brainprint_engine::{
    config::WorkspaceConfig,
    graph::{GraphEndpoint, RelationKind},
    python_semantic::{
        BatchPolicy, LeaseQueries, PyrightInstall, PythonLauncher, PythonSettings, RefreshRequest,
        capability_report, config_basis, lifecycle, refresh_resource, toolchain_identity,
    },
    relations::RelationIndex,
    resource::{ResourceLanguage, ResourceStore},
    runtime::{RequestOptions, RuntimePolicy, SemanticRuntimeSupervisor},
    scan::BaselineScan,
    semantic::{AnalysisContext, AnalysisContextBinding, ProjectRootIdentity, SemanticBackendKind},
    semantic_index::{SemanticIndex, SemanticOwner, SemanticState},
    symbol::SymbolStore,
};

/// The spike install #19 task 5 pinned. Never `PATH`.
fn install_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("scripts")
        .join("python_semantic_spike")
}

fn fixture_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("workspaces")
        .join("python-semantic-spike")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("destination");
    for entry in fs::read_dir(from).expect("read fixture") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy");
        }
    }
}

#[test]
#[ignore = "needs the pinned pyright-typeserver install; see the module docs"]
fn the_real_type_server_resolves_the_task_five_fixture() {
    let root = install_root();
    let Ok(install) = PyrightInstall::locate(&root, "node") else {
        println!(
            "skipped: no pinned {} install under {}",
            brainprint_engine::python_semantic::launcher::PACKAGE,
            root.display()
        );
        return;
    };
    println!("pyright-typeserver {}", install.package_version);

    // A private copy, so the test never writes into the committed
    // fixture.
    let base = env::temp_dir().join(format!("brainprint-pyright-e2e-{}", process::id()));
    let _ = fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    copy_tree(&fixture_source(), &workspace);
    let db_path = base.join("data").join("index.db");
    BaselineScan::open(&db_path)
        .expect("index.db")
        .run_initial_scan(&workspace, &WorkspaceConfig::default(), "workspace-rev-1")
        .expect("baseline scan");

    let settings = PythonSettings::default();
    let context = AnalysisContext {
        workspace: WorkspaceId::from_bytes([11; 16]),
        backend: SemanticBackendKind::Python,
        language: ResourceLanguage::Python,
        project_root: ProjectRootIdentity::Key("python-semantic-spike".to_owned()),
        toolchain: toolchain_identity(
            &install,
            &lifecycle::environment_identity(&workspace, &settings),
        ),
    };
    let binding = AnalysisContextBinding {
        context: context.clone(),
        project_root_rel: String::new(),
        config_file_rel: Some("pyrightconfig.json".to_owned()),
    };

    let supervisor =
        SemanticRuntimeSupervisor::new(RuntimePolicy::default()).with_backend(Arc::new(
            PythonLauncher::new(install, workspace.clone(), settings.clone()),
        ));

    let cold = Instant::now();
    let lease = supervisor
        .acquire(&binding)
        .expect("the type server starts");
    println!("cold start to ready: {:?}", cold.elapsed());
    assert_eq!(supervisor.live_runtime_count(), 1, "exactly one backend");

    let queries = LeaseQueries::new(
        &lease,
        RequestOptions::with_timeout(Duration::from_secs(30)),
    );
    let index = SemanticIndex::open(&db_path).expect("index.db");
    let resources = ResourceStore::open(&db_path).expect("index.db");
    let owner = resources
        .list_active()
        .expect("resources")
        .into_iter()
        .find(|resource| resource.path_key == "pkg/impl.py")
        .expect("pkg/impl.py is indexed");

    let capabilities = capability_report(&context);
    let config = config_basis(&settings, None);
    let refresh_one = |rel: &str| {
        let target = resources
            .list_active()
            .expect("resources")
            .into_iter()
            .find(|resource| resource.path_key == rel)
            .unwrap_or_else(|| panic!("{rel} is indexed"));
        refresh_resource(
            &index,
            &queries,
            &RefreshRequest {
                context: &context,
                workspace_root: &workspace,
                owner: target.id,
                config: &config,
                capabilities: &capabilities,
                // The snapshot really does churn during the backend's
                // initial analysis, so the bound has to be generous.
                policy: BatchPolicy { max_attempts: 12 },
            },
        )
        .unwrap_or_else(|error| panic!("{rel}: {error}"))
    };

    let first = Instant::now();
    let outcome = refresh_one("pkg/impl.py");
    println!(
        "first refresh: {:?}, {} evidence, {} gaps resolved, {} deferred",
        first.elapsed(),
        outcome.evidence_count,
        outcome.merged.gaps_resolved,
        outcome.deferred.len()
    );

    for line in &outcome.report {
        println!("  {line}");
    }
    assert!(outcome.evidence_count > 0);
    assert!(
        outcome.publication.basis.inventory_fingerprint.is_some(),
        "the program-wide snapshot makes the module set part of the basis"
    );
    assert_eq!(
        index
            .status(&SemanticOwner::new(context.context_key(), owner.id))
            .expect("status")
            .state,
        SemanticState::Current
    );

    // The gap task 5 cares about: `x.run(1)` binds to `Base.run`
    // through the receiver's declared type, which no structural tier
    // can do.
    let base_run = SymbolStore::open(&db_path)
        .expect("index.db")
        .list_for_resource(
            resources
                .list_active()
                .expect("resources")
                .into_iter()
                .find(|resource| resource.path_key == "pkg/base.py")
                .expect("pkg/base.py")
                .id,
        )
        .expect("symbols")
        .into_iter()
        .find(|symbol| symbol.name == "run")
        .expect("Base.run");

    let relations = RelationIndex::open(&db_path).expect("index.db");
    let callers = relations
        .callers(&GraphEndpoint::Symbol(base_run.id))
        .expect("callers");
    assert!(
        callers.confirmed_count() > 0,
        "the real backend resolved the receiver-typed call, and `callers` answers it"
    );

    // The dependency call became an external identity, and no
    // dependency file became a Resource.
    let outgoing = relations
        .outgoing(&GraphEndpoint::Resource(owner.id), &[RelationKind::Calls])
        .expect("outgoing");
    let external = outgoing
        .confirmed
        .iter()
        .chain(
            relations
                .outgoing(&GraphEndpoint::Resource(owner.id), &[])
                .expect("all outgoing")
                .confirmed
                .iter(),
        )
        .any(|relation| matches!(relation.target, GraphEndpoint::External(_)));
    println!("external dependency target present: {external}");
    assert!(
        resources
            .list_active()
            .expect("resources")
            .iter()
            .all(|resource| !resource.path_key.contains("typeshed")
                && !resource.path_key.contains("site-packages")),
        "no dependency file is deep-indexed"
    );

    // ---- task 7: the rest of the package -------------------------
    for rel in [
        "pkg/inherit.py",
        "pkg/shapes.py",
        "pkg/twin.py",
        "pkg/uses.py",
    ] {
        let produced = refresh_one(rel);
        println!(
            "{rel}: {} evidence, {} relations created, unproven {:?}",
            produced.evidence_count,
            produced.merged.relations_created,
            produced
                .unproven_overrides
                .iter()
                .map(|unproven| &unproven.reason)
                .collect::<Vec<_>>()
        );
    }

    let symbol_in = |rel: &str, qualified_name: &str| {
        SymbolStore::open(&db_path)
            .expect("index.db")
            .list_for_resource(
                resources
                    .list_active()
                    .expect("resources")
                    .into_iter()
                    .find(|resource| resource.path_key == rel)
                    .unwrap_or_else(|| panic!("{rel}"))
                    .id,
            )
            .expect("symbols")
            .into_iter()
            .find(|symbol| symbol.qualified_name == qualified_name)
            .unwrap_or_else(|| panic!("{qualified_name} in {rel}"))
    };
    let overrides_of = |rel: &str, qualified_name: &str| -> Vec<GraphEndpoint> {
        RelationIndex::open(&db_path)
            .expect("index.db")
            .outgoing(
                &GraphEndpoint::Symbol(symbol_in(rel, qualified_name).id),
                &[RelationKind::Overrides],
            )
            .expect("outgoing")
            .confirmed
            .into_iter()
            .map(|relation| relation.target)
            .collect()
    };

    // OVERRIDES, derived from inheritance that the structural tier and
    // the real backend between them proved.
    assert_eq!(
        overrides_of("pkg/impl.py", "Impl.run"),
        vec![GraphEndpoint::Symbol(
            symbol_in("pkg/base.py", "Base.run").id
        )]
    );
    for member in ["compute", "build", "helper", "label"] {
        assert_eq!(
            overrides_of("pkg/shapes.py", &format!("Concrete.{member}")).len(),
            1,
            "Concrete.{member}"
        );
    }
    assert_eq!(
        overrides_of("pkg/inherit.py", "OnlyOne.only_mixin").len(),
        1,
        "unique across two bases"
    );

    // And the traps: an unrelated same-name method, an ambiguous
    // multiple-inheritance target, and a duck-typed Protocol shape.
    for (rel, name) in [
        ("pkg/twin.py", "Other.run"),
        ("pkg/inherit.py", "Unrelated.run"),
        ("pkg/inherit.py", "Multi.run"),
        ("pkg/shapes.py", "DuckTyped.run"),
    ] {
        assert!(
            overrides_of(rel, name).is_empty(),
            "{name} must not override anything"
        );
    }
    let count = |sql: &str| -> i64 {
        SemanticIndex::open(&db_path)
            .expect("index.db")
            .connection()
            .query_row(sql, [], |row| row.get(0))
            .expect("count")
    };
    assert_eq!(
        count("SELECT COUNT(*) FROM relation WHERE kind = 'IMPLEMENTS'"),
        0,
        "no Protocol conformance is manufactured"
    );

    // Every relation kind task 7 owes, in the one canonical graph.
    let kinds: Vec<String> = SemanticIndex::open(&db_path)
        .expect("index.db")
        .connection()
        .prepare("SELECT DISTINCT kind FROM relation ORDER BY kind")
        .expect("prepare")
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("kinds");
    println!("relation kinds: {kinds:?}");
    for required in [
        "CALLS",
        "EXTENDS",
        "IMPORTS",
        "OVERRIDES",
        "REFERENCES",
        "USES_TYPE",
    ] {
        assert!(
            kinds.iter().any(|kind| kind == required),
            "{required} missing from {kinds:?}"
        );
    }

    // Corrected inheritance ownership: the subclass is the source,
    // never the file, and a qualified base is resolved rather than
    // missing.
    let extends_of = |rel: &str, qualified_name: &str| -> Vec<GraphEndpoint> {
        RelationIndex::open(&db_path)
            .expect("index.db")
            .outgoing(
                &GraphEndpoint::Symbol(symbol_in(rel, qualified_name).id),
                &[RelationKind::Extends],
            )
            .expect("outgoing")
            .confirmed
            .into_iter()
            .map(|relation| relation.target)
            .collect()
    };
    assert_eq!(
        extends_of("pkg/impl.py", "Impl"),
        vec![GraphEndpoint::Symbol(symbol_in("pkg/base.py", "Base").id)]
    );
    assert_eq!(
        count(
            "SELECT COUNT(*) FROM relation \
             JOIN graph_entity ON graph_entity.id = relation.source_entity_id \
             WHERE relation.kind = 'EXTENDS' AND graph_entity.entity_kind = 'RESOURCE'"
        ),
        0,
        "no inheritance relation is sourced from a file"
    );
    // `class Qualified(base.Base)` -- an exact site the backend proves.
    assert_eq!(
        extends_of("pkg/inherit.py", "Qualified"),
        vec![GraphEndpoint::Symbol(symbol_in("pkg/base.py", "Base").id)]
    );
    // `class Abstract(abc.ABC)` -- a dependency base, as an external
    // identity, with nothing deep-indexed to get it.
    let abstract_bases = extends_of("pkg/shapes.py", "Abstract");
    assert_eq!(abstract_bases.len(), 1);
    assert!(
        matches!(abstract_bases[0], GraphEndpoint::External(_)),
        "unexpected: {abstract_bases:?}"
    );
    // A nested class owns its own base.
    assert_eq!(
        extends_of("pkg/inherit.py", "Outer.Inner"),
        vec![GraphEndpoint::Symbol(
            symbol_in("pkg/inherit.py", "Mixin").id
        )]
    );
    // And the derivation that the corrected ownership unblocks.
    assert_eq!(
        overrides_of("pkg/inherit.py", "Qualified.run").len(),
        1,
        "a qualified base now carries its override through"
    );

    // Exact evidence spans: the override is anchored on the name token
    // of the overriding declaration, so a reader gets source.
    let impl_run = symbol_in("pkg/impl.py", "Impl.run");
    let anchored: (i64, i64) = SemanticIndex::open(&db_path)
        .expect("index.db")
        .connection()
        .query_row(
            "SELECT occurrence.start_byte, occurrence.end_byte \
             FROM semantic_evidence \
             JOIN occurrence ON occurrence.id = semantic_evidence.occurrence_id \
             WHERE semantic_evidence.capability = 'OVERRIDES' \
               AND occurrence.containing_symbol_id = \
                   (SELECT id FROM symbol WHERE uid = ?1)",
            rusqlite::params![impl_run.id.to_bytes().to_vec()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("the override evidence row");
    let impl_source = fs::read_to_string(workspace.join("pkg/impl.py")).expect("source");
    let start = usize::try_from(anchored.0).expect("fits");
    let end = usize::try_from(anchored.1).expect("fits");
    assert_eq!(&impl_source[start..end], "run");

    // A second refresh over an unchanged Workspace changes nothing.
    let warm = Instant::now();
    let again = refresh_one("pkg/impl.py");
    println!("warm refresh: {:?}", warm.elapsed());
    assert_eq!(again.merged.relations_created, 0, "idempotent");
    assert_eq!(
        again.merged.relations_removed, 0,
        "and not self-withdrawing"
    );
    assert_eq!(
        overrides_of("pkg/impl.py", "Impl.run").len(),
        1,
        "and the derived edge survives an identical refresh"
    );

    drop(lease);
    supervisor.shutdown();
    let _ = fs::remove_dir_all(&base);
}

/// The task 8 lifecycle, end to end, against the real backend.
///
/// What a fake host cannot show: that Pyright actually notices a
/// `workspace/didChangeWatchedFiles` batch and answers differently
/// afterwards, without a single document overlay being sent.
#[test]
#[ignore = "needs the pinned pyright-typeserver install; see the module docs"]
fn the_real_backend_follows_a_workspace_change_through_the_lifecycle() {
    let root = install_root();
    let Ok(install) = PyrightInstall::locate(&root, "node") else {
        println!("skipped: no pinned install under {}", root.display());
        return;
    };

    let base = env::temp_dir().join(format!("brainprint-pyright-lifecycle-{}", process::id()));
    let _ = fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    copy_tree(&fixture_source(), &workspace);
    let db_path = base.join("data").join("index.db");
    BaselineScan::open(&db_path)
        .expect("index.db")
        .run_initial_scan(&workspace, &WorkspaceConfig::default(), "workspace-rev-1")
        .expect("baseline scan");

    let settings = PythonSettings::default();
    let environment = lifecycle::environment_identity(&workspace, &settings);
    let context = AnalysisContext {
        workspace: WorkspaceId::from_bytes([13; 16]),
        backend: SemanticBackendKind::Python,
        language: ResourceLanguage::Python,
        project_root: ProjectRootIdentity::Key("python-semantic-spike".to_owned()),
        toolchain: toolchain_identity(&install, &environment),
    };
    let binding = AnalysisContextBinding {
        context: context.clone(),
        project_root_rel: String::new(),
        config_file_rel: Some("pyrightconfig.json".to_owned()),
    };
    let supervisor =
        SemanticRuntimeSupervisor::new(RuntimePolicy::default()).with_backend(Arc::new(
            PythonLauncher::new(install, workspace.clone(), settings.clone()),
        ));
    let lease = supervisor
        .acquire(&binding)
        .expect("the type server starts");
    let queries = LeaseQueries::new(
        &lease,
        RequestOptions::with_timeout(Duration::from_secs(30)),
    );

    // The configuration the backend will actually use, discovered the
    // same way the lifecycle does.
    let index = SemanticIndex::open(&db_path).expect("index.db");
    let discovered =
        lifecycle::discover_config(index.connection(), &workspace, "").expect("discover config");
    assert_eq!(discovered.source, lifecycle::ConfigSource::PyrightConfig);
    let config = discovered.basis(&settings);
    let capabilities = capability_report(&context);

    let resources = ResourceStore::open(&db_path).expect("index.db");
    let resource_of = |rel: &str| {
        resources
            .list_active()
            .expect("resources")
            .into_iter()
            .find(|resource| resource.path_key == rel)
            .unwrap_or_else(|| panic!("{rel}"))
    };
    let refresh_one = |rel: &str| {
        refresh_resource(
            &index,
            &queries,
            &RefreshRequest {
                context: &context,
                workspace_root: &workspace,
                owner: resource_of(rel).id,
                config: &config,
                capabilities: &capabilities,
                policy: BatchPolicy { max_attempts: 12 },
            },
        )
        .unwrap_or_else(|error| panic!("{rel}: {error}"))
    };

    refresh_one("pkg/impl.py");
    let impl_owner = SemanticOwner::new(context.context_key(), resource_of("pkg/impl.py").id);
    assert_eq!(
        index.status(&impl_owner).expect("status").state,
        SemanticState::Current
    );
    let overrides = |rel: &str, name: &str| -> usize {
        let symbol = SymbolStore::open(&db_path)
            .expect("index.db")
            .list_for_resource(resource_of(rel).id)
            .expect("symbols")
            .into_iter()
            .find(|symbol| symbol.qualified_name == name)
            .unwrap_or_else(|| panic!("{name}"));
        RelationIndex::open(&db_path)
            .expect("index.db")
            .outgoing(
                &GraphEndpoint::Symbol(symbol.id),
                &[RelationKind::Overrides],
            )
            .expect("outgoing")
            .confirmed
            .len()
    };
    assert_eq!(overrides("pkg/impl.py", "Impl.run"), 1);

    // --- the change: `Base.run` is renamed, and impl.py never moves.
    let affected: std::collections::BTreeSet<SemanticOwner> = index
        .owners_depending_on(resource_of("pkg/base.py").id)
        .expect("owners")
        .into_iter()
        .collect();
    assert!(affected.contains(&impl_owner), "affected: {affected:?}");

    // 1. withdraw before any destructive structural work.
    lifecycle::withdraw_affected(&index, &affected, "SEMANTIC_SOURCE_MOVED").expect("withdraw");
    assert_eq!(overrides("pkg/impl.py", "Impl.run"), 0);

    // 2. the filesystem holds the new truth before the backend is told.
    fs::write(
        workspace.join("pkg/base.py"),
        "class Base:\n    def renamed(self, value: int) -> str:\n        ...\n",
    )
    .expect("rewrite");

    // 3. structural replacement.
    BaselineScan::open(&db_path)
        .expect("index.db")
        .run_initial_scan(&workspace, &WorkspaceConfig::default(), "workspace-rev-2")
        .expect("structural replacement");

    // 4. one deterministic notification batch, and no overlay.
    let changes = vec![lifecycle::ResourceChange::new(
        resource_of("pkg/base.py").id,
        lifecycle::ChangeKind::Changed,
        "pkg/base.py",
    )];
    let watched = lifecycle::watched_changes(&workspace, &changes);
    assert_eq!(watched.len(), 1);
    brainprint_engine::python_semantic::adapter::notify_watched_files(&queries, watched)
        .expect("notify");

    // 5. refresh only the affected owner, and the edge does not return
    //    because the member it pointed at is gone.
    let after = refresh_one("pkg/impl.py");
    println!(
        "after rename: {} evidence, {} created, {} removed",
        after.evidence_count, after.merged.relations_created, after.merged.relations_removed
    );
    assert_eq!(
        overrides("pkg/impl.py", "Impl.run"),
        0,
        "the real backend agrees the ancestor member is gone"
    );

    // Put it back and the targeted refresh restores the edge.
    fs::write(
        workspace.join("pkg/base.py"),
        "class Base:\n    def run(self, value: int) -> str:\n        ...\n",
    )
    .expect("restore");
    let affected: std::collections::BTreeSet<SemanticOwner> = index
        .owners_depending_on(resource_of("pkg/base.py").id)
        .expect("owners")
        .into_iter()
        .collect();
    lifecycle::withdraw_affected(&index, &affected, "SEMANTIC_SOURCE_MOVED").expect("withdraw");
    BaselineScan::open(&db_path)
        .expect("index.db")
        .run_initial_scan(&workspace, &WorkspaceConfig::default(), "workspace-rev-3")
        .expect("structural replacement");
    brainprint_engine::python_semantic::adapter::notify_watched_files(
        &queries,
        lifecycle::watched_changes(&workspace, &changes),
    )
    .expect("notify");
    refresh_one("pkg/impl.py");
    assert_eq!(
        overrides("pkg/impl.py", "Impl.run"),
        1,
        "and it comes back when the proof does"
    );

    // Idempotent: the same refresh again changes nothing.
    let again = refresh_one("pkg/impl.py");
    assert_eq!(again.merged.relations_created, 0);
    assert_eq!(again.merged.relations_removed, 0);

    // A reopen with an unchanged basis needs no process at all.
    let inventory = brainprint_engine::python_semantic::inventory_fingerprint(
        index.connection(),
        ResourceLanguage::Python,
    )
    .expect("inventory");
    // ... but only when the environment those inputs name can be
    // proven. This fixture configures no venv, so it cannot be, and a
    // persisted publication must not restore itself CURRENT off an
    // equality nobody verified (#19 task 8 corrective patch).
    let unproven =
        lifecycle::current_inputs(&context, &config, &capabilities, &inventory, &environment);
    assert!(
        !environment.assurance.is_proven(),
        "no venv is configured: {:?}",
        environment.assurance
    );
    let answers = lifecycle::revalidate_context(&index, &context, &unproven).expect("revalidate");
    assert!(
        answers
            .iter()
            .any(|(owner, status)| owner == &impl_owner && !status.is_current()),
        "an unprovable environment owes a refresh: {answers:?}"
    );

    // The basis itself is untouched: with the environment proven, the
    // same reopen restores CURRENT with no process involved.
    let answers = lifecycle::revalidate_context(
        &index,
        &context,
        &unproven.clone().with_environment_proven(true),
    )
    .expect("revalidate");
    assert!(
        answers
            .iter()
            .any(|(owner, status)| owner == &impl_owner && status.is_current()),
        "revalidated: {answers:?}"
    );

    drop(lease);
    supervisor.shutdown();
    let _ = fs::remove_dir_all(&base);
}
