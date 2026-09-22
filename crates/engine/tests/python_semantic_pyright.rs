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
        capability_report, config_basis, refresh_resource, toolchain_identity,
    },
    relations::RelationIndex,
    resource::{ResourceLanguage, ResourceStore},
    runtime::{RequestOptions, RuntimePolicy, SemanticRuntimeSupervisor},
    scan::BaselineScan,
    semantic::{AnalysisContext, AnalysisContextBinding, ProjectRootIdentity, SemanticBackendKind},
    semantic_index::{SemanticIndex, SemanticState},
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
        toolchain: toolchain_identity(&install, &settings),
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
    let first = Instant::now();
    let outcome = refresh_resource(
        &index,
        &queries,
        &RefreshRequest {
            context: &context,
            workspace_root: &workspace,
            owner: owner.id,
            config: &config,
            capabilities: &capabilities,
            // The snapshot really does churn during the backend's
            // initial analysis, so the bound has to be generous here.
            policy: BatchPolicy { max_attempts: 12 },
        },
    )
    .expect("a real semantic refresh");
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
        index.status(&context.context_key()).expect("status").state,
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

    // A second refresh over an unchanged Workspace changes nothing.
    let warm = Instant::now();
    let again = refresh_resource(
        &index,
        &queries,
        &RefreshRequest {
            context: &context,
            workspace_root: &workspace,
            owner: owner.id,
            config: &config,
            capabilities: &capabilities,
            policy: BatchPolicy { max_attempts: 12 },
        },
    )
    .expect("a second refresh");
    println!("warm refresh: {:?}", warm.elapsed());
    assert_eq!(again.merged.relations_created, 0, "idempotent");
    assert_eq!(
        again.merged.relations_removed, 0,
        "and not self-withdrawing"
    );

    drop(lease);
    supervisor.shutdown();
    let _ = fs::remove_dir_all(&base);
}
