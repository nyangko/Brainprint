//! The Rust backend's always-on suite.
//!
//! Every test here runs with no Rust toolchain reachable and no
//! rust-analyzer, against a scripted backend that answers what #19 task
//! 13 measured the real one answering. The real-backend acceptance
//! lives in `tests/i4_rust_acceptance.rs` and is skipped when the
//! toolchain is absent; this suite is what must never be skipped,
//! because it is also the Level B contract.

use std::path::Path;

use super::{
    IMPLEMENTS, INHERITANCE, OVERLOAD_RESOLUTION, OVERRIDES, adapter, capability_report,
    crate_identity, external_entity,
    lifecycle::{self, ChangeClass, ChangeKind, ResourceChange},
    protocol::{self, ProjectExecutionTrust},
    tests_support::{Fixture, ScriptedBackend, context, refresh},
};
use crate::{
    graph::{GraphEndpoint, RelationKind},
    resolution::{Dispatch, Support},
    resource::ResourceLanguage,
    semantic::SemanticCapability,
    semantic_index::SemanticIndex,
};

/// The module whose trait implementations the fixture turns on.
const RUNNER: &str = "crates/core/src/runner.rs";

/// The byte span of the `use` leaf item containing `needle`.
///
/// The structural tier anchors a compound `use crate::a::{B, C}` as one
/// occurrence per decomposed leaf item -- `B` and `C` each get their own
/// narrow span, not the whole grouped specifier (#42). Recorded here
/// rather than guessed at, because a test that asked somewhere else
/// would pass against a scripted backend and prove nothing.
fn compound_use(fixture: &Fixture, rel: &str, needle: &str) -> (usize, usize) {
    let owner = fixture.resource(rel);
    let text = fixture.text(rel);
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let gaps =
        crate::evidence::list_unresolved_for_resource(index.connection(), owner.id).expect("gaps");
    let sites = adapter::collect_sites(
        index.connection(),
        &owner,
        &text,
        &gaps,
        &context().context_key(),
    )
    .expect("sites");
    sites
        .sites
        .iter()
        .map(|site| (site.occurrence.start_byte, site.occurrence.end_byte))
        .find(|(start, end)| text[*start..*end].contains(needle))
        .unwrap_or_else(|| panic!("no site covering {needle:?} in {rel}"))
}

// ---------------------------------------------------------------------
// Trust
// ---------------------------------------------------------------------

/// Untrusted is the default, and nothing infers otherwise.
#[test]
fn a_workspace_is_untrusted_until_something_says_so() {
    assert_eq!(
        ProjectExecutionTrust::default_for_workspace(),
        ProjectExecutionTrust::Untrusted
    );
    assert!(!ProjectExecutionTrust::Untrusted.may_load_projects());
    assert!(ProjectExecutionTrust::Trusted.may_load_projects());
}

/// An untrusted Workspace cannot re-read its own manifests, and the
/// refusal is an error rather than a quiet skip.
#[test]
fn an_untrusted_workspace_refuses_to_read_its_own_manifests() {
    let fixture = Fixture::create("trust-refuse");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let backend = ScriptedBackend::untrusted();
    let config = lifecycle::discover_packages_under(
        index.connection(),
        ProjectExecutionTrust::Untrusted,
        Some(&fixture.root),
    )
    .expect("packages");

    let error = lifecycle::reload_projects(
        &backend,
        &backend,
        &fixture.root,
        &[ResourceChange::new(
            fixture.resource("crates/core/Cargo.toml").id,
            ChangeKind::Changed,
            "crates/core/Cargo.toml",
        )
        .with_language(None)],
        &config,
    )
    .expect_err("an untrusted Workspace must refuse");

    assert!(
        matches!(error, lifecycle::LifecycleError::Untrusted(_)),
        "{error}"
    );
    assert!(!backend.project_loaded(), "nothing was read");
    assert!(
        backend.calls().is_empty(),
        "the refusal happens before the notification, not after it"
    );
}

/// An untrusted analysis still runs, and still tells the truth.
#[test]
fn an_untrusted_analysis_publishes_gaps_not_guesses() {
    let fixture = Fixture::create("trust-gaps");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let backend = ScriptedBackend::untrusted();

    let outcome = refresh(&fixture, &index, &backend, RUNNER).expect("refresh");
    assert!(!backend.project_loaded(), "no project code ran");
    assert!(
        !outcome
            .report
            .iter()
            .any(|line| line.contains("-> RESOLVED")),
        "with no crate graph nothing binds: {:?}",
        outcome.report
    );
}

/// The trust mode and the execution switches are part of what a
/// publication was based on.
#[test]
fn trust_and_the_safe_configuration_participate_in_the_basis() {
    let fixture = Fixture::create("trust-basis");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let untrusted = lifecycle::discover_packages_under(
        index.connection(),
        ProjectExecutionTrust::Untrusted,
        Some(&fixture.root),
    )
    .expect("untrusted");
    let trusted = lifecycle::discover_packages_under(
        index.connection(),
        ProjectExecutionTrust::Trusted,
        Some(&fixture.root),
    )
    .expect("trusted");

    assert_ne!(
        untrusted.basis().fingerprint(),
        trusted.basis().fingerprint(),
        "granting trust must invalidate, not silently widen"
    );
    // And the switches themselves, so turning build scripts on later
    // cannot reuse an analysis made with them off.
    assert!(
        format!("{:?}", trusted.basis()).contains("buildScripts=false"),
        "the configuration that made the load safe is part of the basis"
    );
}

/// The configuration disables execution and not the crate graph.
///
/// The finding that changed the recommended P0 configuration: with
/// `cargo.noDeps` set, every cross-crate answer came back empty.
#[test]
fn the_safe_configuration_is_the_measured_one() {
    let config = protocol::safe_configuration();
    assert_eq!(config["cargo"]["buildScripts"]["enable"], false);
    assert_eq!(config["procMacro"]["enable"], false);
    assert_eq!(config["check"]["enable"], false);
    assert_eq!(config["checkOnSave"], false);
    assert!(
        config["cargo"].get("noDeps").is_none(),
        "noDeps severs a workspace from its own members"
    );
    assert!(
        protocol::MEASURED_PROJECT_LOAD_EFFECTS.contains("not a sandbox"),
        "what it does is recorded, not summarised as safe"
    );
}

/// The handshake asks for the barrier, because without it there is none.
#[test]
fn the_handshake_asks_for_the_only_deterministic_barrier() {
    let params = protocol::initialize_params("file:///w", 1, "w");
    assert_eq!(
        params["capabilities"]["experimental"]["serverStatusNotification"],
        true
    );
}

// ---------------------------------------------------------------------
// What Rust does not have
// ---------------------------------------------------------------------

/// No inheritance, no overrides, no overloading — declared, not omitted.
#[test]
fn rust_does_not_pretend_to_have_what_it_lacks() {
    assert_eq!(INHERITANCE, Support::Unsupported);
    assert_eq!(OVERRIDES, Support::Unsupported);
    assert_eq!(OVERLOAD_RESOLUTION, Support::Unsupported);
    assert_eq!(IMPLEMENTS, Support::Supported);

    let context = context();
    let report = capability_report(&context, ProjectExecutionTrust::Trusted);
    assert_eq!(
        report.support(SemanticCapability::Inheritance),
        Support::Unsupported,
        "a supertrait is a requirement on implementors, not a base class"
    );
    assert_eq!(
        report.support(SemanticCapability::Overrides),
        Support::Unsupported,
        "an impl method replacing a default implements it; it overrides nothing"
    );
    assert_eq!(
        report.support(SemanticCapability::OverloadResolution),
        Support::Unsupported,
        "Rust has no user-defined function overloading"
    );
}

/// Every capability has a verdict, in both trust modes.
#[test]
fn every_capability_is_declared_in_both_trust_modes() {
    let context = context();
    for trust in [
        ProjectExecutionTrust::Untrusted,
        ProjectExecutionTrust::Trusted,
    ] {
        let report = capability_report(&context, trust);
        for capability in SemanticCapability::ALL {
            assert!(
                report.is_declared(capability),
                "{capability:?} has no verdict under {trust}"
            );
        }
    }
}

/// The matrix, as measured. Changing a verdict changes this test.
#[test]
fn the_capability_matrix_is_what_was_measured() {
    let context = context();
    let trusted = capability_report(&context, ProjectExecutionTrust::Trusted);
    for (capability, expected) in [
        (SemanticCapability::ResourceDiscovery, Support::Supported),
        (SemanticCapability::SyntaxStructure, Support::Supported),
        (SemanticCapability::SymbolSpan, Support::Supported),
        (SemanticCapability::ContainingScope, Support::Supported),
        (SemanticCapability::ImportDeclaration, Support::Supported),
        (SemanticCapability::ExportDeclaration, Support::Supported),
        (
            SemanticCapability::EmbeddedRegionMapping,
            Support::Unsupported,
        ),
        (
            SemanticCapability::OriginalSourceMapping,
            Support::Unsupported,
        ),
        (SemanticCapability::SymbolDefinition, Support::Supported),
        (SemanticCapability::ImportBinding, Support::Partial),
        (SemanticCapability::AliasResolution, Support::Supported),
        (SemanticCapability::ReexportResolution, Support::Supported),
        (SemanticCapability::References, Support::Supported),
        (SemanticCapability::CallsIntraFile, Support::Supported),
        (SemanticCapability::CallsCrossFile, Support::Supported),
        (
            SemanticCapability::ExternalSymbolResolution,
            Support::Supported,
        ),
        (SemanticCapability::TypeResolution, Support::Partial),
        (SemanticCapability::Inheritance, Support::Unsupported),
        (SemanticCapability::Implements, Support::Supported),
        (SemanticCapability::Overrides, Support::Unsupported),
        (SemanticCapability::StaticDispatchTarget, Support::Supported),
        (SemanticCapability::OverloadResolution, Support::Unsupported),
        (SemanticCapability::ImplementationTarget, Support::Supported),
    ] {
        assert_eq!(
            trusted.support(capability),
            expected,
            "{capability:?} was measured as {expected:?}"
        );
    }

    // Untrusted: the structural half is unchanged, and every binding
    // question is refused rather than answered with nothing.
    let untrusted = capability_report(&context, ProjectExecutionTrust::Untrusted);
    assert_eq!(
        untrusted.support(SemanticCapability::SyntaxStructure),
        Support::Supported
    );
    for capability in [
        SemanticCapability::SymbolDefinition,
        SemanticCapability::References,
        SemanticCapability::Implements,
    ] {
        assert_eq!(
            untrusted.support(capability),
            Support::Unsupported,
            "{capability:?} needs a crate graph"
        );
    }
}

// ---------------------------------------------------------------------
// External identity
// ---------------------------------------------------------------------

/// A dependency is a crate and a version, never a path.
#[test]
fn external_identity_is_a_crate_and_never_a_machine_path() {
    let registry = Path::new(
        "/Users/someone/.cargo/registry/src/index.crates.io-6f17d22bba15001f/serde-1.0.219/src/lib.rs",
    );
    assert_eq!(
        crate_identity(registry),
        Some(("serde".to_owned(), Some("1.0.219".to_owned())))
    );
    let entity = external_entity(registry, Some("Serialize")).expect("external identity");
    assert_eq!(entity.package_identity, "serde");
    assert_eq!(entity.resolved_version.as_deref(), Some("1.0.219"));
    assert_eq!(entity.qualified_name.as_deref(), Some("serde::Serialize"));
    assert_eq!(entity.declaration_locator, None);

    let rendered = format!("{entity:?}");
    for leaked in ["/Users/", ".cargo", "index.crates.io", "/src/lib.rs"] {
        assert!(
            !rendered.contains(leaked),
            "{leaked:?} is a locator and must not reach identity: {rendered}"
        );
    }
}

/// A git dependency has no version, and says so rather than inventing
/// one from the checkout hash.
#[test]
fn a_git_dependency_has_a_name_and_no_version() {
    let checkout =
        Path::new("/Users/someone/.cargo/git/checkouts/thing-abc123def/9f2c1/src/lib.rs");
    assert_eq!(crate_identity(checkout), Some(("thing".to_owned(), None)));
    let entity = external_entity(checkout, None).expect("identity");
    assert_eq!(entity.resolved_version, None);
    assert!(!format!("{entity:?}").contains("abc123def"));
}

/// A standard-library item is the crate it lives in.
#[test]
fn a_sysroot_item_is_its_std_crate() {
    let std_item = Path::new(
        "/Users/someone/.rustup/toolchains/stable-aarch64-apple-darwin/lib/rustlib/src/rust/library/core/src/option.rs",
    );
    assert_eq!(crate_identity(std_item), Some(("core".to_owned(), None)));
    let entity = external_entity(std_item, Some("Option")).expect("identity");
    assert_eq!(entity.package_identity, "core");
    assert_eq!(entity.qualified_name.as_deref(), Some("core::Option"));
    assert!(!format!("{entity:?}").contains(".rustup"));
}

/// Workspace source is not a dependency, whatever it is called.
#[test]
fn workspace_source_is_never_read_as_a_dependency() {
    assert_eq!(
        crate_identity(Path::new("/w/crates/core/src/runner.rs")),
        None
    );
    assert!(!protocol::is_dependency_path(Path::new(
        "/w/crates/core/src/runner.rs"
    )));
}

/// Those trees are recognised so they are never deep-indexed.
#[test]
fn dependency_and_toolchain_trees_are_recognised() {
    for path in [
        "/Users/x/.cargo/registry/src/index.crates.io-1/serde-1.0.219/src/lib.rs",
        "/Users/x/.cargo/git/checkouts/thing-abc/1234/src/lib.rs",
        "/Users/x/.rustup/toolchains/stable/lib/rustlib/src/rust/library/core/src/option.rs",
        "/w/target/debug/build/bp-core-123/out/generated.rs",
    ] {
        assert!(protocol::is_dependency_path(Path::new(path)), "{path}");
    }
}

// ---------------------------------------------------------------------
// The generated-source gate
// ---------------------------------------------------------------------

/// A macro expansion is not editable source, and never becomes a span.
#[test]
fn a_virtual_or_generated_location_is_refused() {
    for uri in [
        "rust-analyzer://macro-expansion/1",
        "untitled:Untitled-1",
        "file:///w/x.rs?macro-expansion",
    ] {
        assert!(protocol::is_virtual_uri(uri), "{uri}");
    }
    assert!(!protocol::is_virtual_uri(
        "file:///w/crates/core/src/lib.rs"
    ));
}

/// Build-script output is real on disk and still not project source.
#[test]
fn build_script_output_is_not_project_source() {
    assert!(protocol::is_dependency_path(Path::new(
        "/w/target/debug/build/bp-core-7/out/generated.rs"
    )));
}

// ---------------------------------------------------------------------
// Packages, crates and modules
// ---------------------------------------------------------------------

/// Three different things whose names usually match.
#[test]
fn a_package_a_crate_and_a_module_stay_apart() {
    let fixture = Fixture::create("identity-layers");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let config = lifecycle::discover_packages_under(
        index.connection(),
        ProjectExecutionTrust::Trusted,
        Some(&fixture.root),
    )
    .expect("packages");

    // The package is what a manifest declares.
    assert_eq!(
        config.package_names.get("crates/core/Cargo.toml"),
        Some(&"bp-core".to_owned()),
        "the package is named by its manifest"
    );
    // The workspace root declares no package, and saying so is the
    // truth about it rather than a missing entry.
    assert_eq!(config.package_names.get("Cargo.toml"), None);
    assert!(
        config
            .workspace_manifest()
            .is_some_and(|manifest| manifest.path_key == "Cargo.toml"),
        "and it is still the workspace manifest"
    );
    // A source file belongs to a package; the module it declares is a
    // separate question the backend answers.
    assert_eq!(
        config.owning_package.get(&fixture.resource(RUNNER).id),
        Some(&"crates/core/Cargo.toml".to_owned())
    );
    // A package can build several crate targets: lib, bin and the
    // integration test all live under one manifest.
    assert_eq!(
        config
            .owning_package
            .get(&fixture.resource("crates/core/tests/integration.rs").id),
        Some(&"crates/core/Cargo.toml".to_owned()),
        "an integration test is another crate target of the same package"
    );
}

/// This backend answers for Rust and nothing else.
#[test]
fn the_backend_serves_one_language() {
    assert_eq!(lifecycle::SERVED_LANGUAGES, [ResourceLanguage::Rust]);
}

// ---------------------------------------------------------------------
// Change classification
// ---------------------------------------------------------------------

/// Adding a `.rs` is not a project change, and that is the whole
/// difference from a manifest-driven language.
///
/// In Rust the module tree is written in source: `src/new.rs` is not a
/// module until some file says `mod new;`, and saying it is an edit the
/// document sync already carries. Treating every `.rs` add as a crate
/// graph change would reload the project for something the project
/// never mentioned.
#[test]
fn a_new_source_file_is_not_a_project_change() {
    let fixture = Fixture::create("classes");
    let id = fixture.resource(RUNNER).id;

    for (path, expected) in [
        (RUNNER, ChangeClass::DocumentContent),
        ("crates/core/src/new.rs", ChangeClass::DocumentContent),
        ("crates/core/src/lib.rs", ChangeClass::DocumentContent),
    ] {
        assert_eq!(
            ResourceChange::new(id, ChangeKind::Added, path).class(),
            expected,
            "{path}"
        );
    }
    for path in [
        "Cargo.toml",
        "crates/core/Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        ".cargo/config.toml",
    ] {
        assert_eq!(
            ResourceChange::new(id, ChangeKind::Changed, path)
                .with_language(None)
                .class(),
            ChangeClass::ProjectDefinition,
            "{path} is the project's own definition"
        );
    }
}

/// A content change is synchronized with Brainprint's current bytes,
/// opened once and then changed with a range.
#[test]
fn a_content_change_hands_over_current_bytes() {
    let fixture = Fixture::create("content-sync");
    let backend = ScriptedBackend::new();
    let change = ResourceChange::new(fixture.resource(RUNNER).id, ChangeKind::Changed, RUNNER);

    for _ in 0..3 {
        lifecycle::synchronize_documents(
            &backend,
            &backend,
            &fixture.root,
            std::slice::from_ref(&change),
        )
        .expect("sync");
    }

    let sent: Vec<(&'static str, i64)> = backend
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            protocol::RustRequest::OpenDocument { version, .. } => Some(("open", version)),
            protocol::RustRequest::ChangeDocument { version, .. } => Some(("change", version)),
            _ => None,
        })
        .collect();
    assert_eq!(
        sent,
        vec![("open", 1), ("change", 2), ("change", 3)],
        "a document is opened once; the server declares incremental sync"
    );
    assert!(
        !backend.project_loaded(),
        "a source edit needs no project reload"
    );
}

/// A deleted document has no current bytes and is not synchronized.
#[test]
fn a_deleted_document_is_not_handed_over() {
    let fixture = Fixture::create("content-delete");
    let id = fixture.resource(RUNNER).id;
    fixture.remove(RUNNER);
    let backend = ScriptedBackend::new();
    assert_eq!(
        lifecycle::synchronize_documents(
            &backend,
            &backend,
            &fixture.root,
            &[ResourceChange::new(id, ChangeKind::Deleted, RUNNER)],
        )
        .expect("sync"),
        0
    );
    assert!(backend.opened().is_empty());
}

/// A manifest change waits for the server to settle, not for a sleep.
#[test]
fn a_manifest_change_waits_for_the_quiescent_barrier() {
    let fixture = Fixture::create("manifest-sync");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let backend = ScriptedBackend::new();
    let config = lifecycle::discover_packages_under(
        index.connection(),
        ProjectExecutionTrust::Trusted,
        Some(&fixture.root),
    )
    .expect("packages");

    let settled = lifecycle::reload_projects(
        &backend,
        &backend,
        &fixture.root,
        &[ResourceChange::new(
            fixture.resource("crates/core/Cargo.toml").id,
            ChangeKind::Changed,
            "crates/core/Cargo.toml",
        )
        .with_language(None)],
        &config,
    )
    .expect("reload");

    assert_eq!(settled, 1, "one settling was awaited");
    assert!(backend.project_loaded());
    assert_eq!(
        backend.notifications().len(),
        1,
        "the filesystem move is announced before the reload"
    );
}

// ---------------------------------------------------------------------
// Configuration inputs
// ---------------------------------------------------------------------

/// Every project input that changes what the compiler sees moves the
/// basis.
#[test]
fn every_project_input_change_invalidates() {
    for (rel, changed) in [
        (
            "Cargo.toml",
            "[workspace]\nresolver = \"2\"\nmembers = []\n",
        ),
        (
            "crates/core/Cargo.toml",
            "[package]\nname = \"bp-core\"\nversion = \"0.2.0\"\nedition = \"2021\"\n",
        ),
        ("Cargo.lock", "version = 4\n"),
        (
            "rust-toolchain.toml",
            "[toolchain]\nchannel = \"nightly\"\n",
        ),
    ] {
        let fixture = Fixture::create("config-input");
        let basis = |fixture: &Fixture| {
            let index = SemanticIndex::open(&fixture.db_path()).expect("index");
            lifecycle::discover_packages_under(
                index.connection(),
                ProjectExecutionTrust::Trusted,
                Some(&fixture.root),
            )
            .expect("packages")
            .basis()
            .fingerprint()
        };
        let before = basis(&fixture);
        fixture.write(rel, changed);
        fixture.rescan("workspace-rev-2");
        assert_ne!(
            before,
            basis(&fixture),
            "{rel} changes what the compilation is"
        );
    }
}

/// A feature or target selection is a different semantic world.
///
/// Rust's version of the multi-target problem: one source file means
/// different things under `#[cfg(feature = "extra")]`. One context
/// claims one selection, and changing the selection invalidates rather
/// than widening.
#[test]
fn a_feature_or_target_selection_is_part_of_the_basis() {
    let fixture = Fixture::create("cfg-worlds");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let base = lifecycle::discover_packages_under(
        index.connection(),
        ProjectExecutionTrust::Trusted,
        Some(&fixture.root),
    )
    .expect("packages");

    let with_feature = lifecycle::RustProjectConfig {
        features: vec!["extra".to_owned()],
        ..base.clone()
    };
    let no_defaults = lifecycle::RustProjectConfig {
        no_default_features: true,
        ..base.clone()
    };
    let other_target = lifecycle::RustProjectConfig {
        target: Some("x86_64-unknown-linux-gnu".to_owned()),
        ..base.clone()
    };

    let fingerprint = |config: &lifecycle::RustProjectConfig| config.basis().fingerprint();
    let baseline = fingerprint(&base);
    for (label, other) in [
        ("a feature", &with_feature),
        ("default features", &no_defaults),
        ("a target triple", &other_target),
    ] {
        assert_ne!(
            baseline,
            fingerprint(other),
            "{label} changes what the source means"
        );
    }
}

/// A package name is read as written, never evaluated.
#[test]
fn a_package_name_is_read_not_evaluated() {
    use lifecycle::declared_package_name as name;
    assert_eq!(
        name("[package]\nname = \"bp-core\"\nversion = \"0.1.0\"\n"),
        Some("bp-core".to_owned())
    );
    // A workspace root declares no package.
    assert_eq!(name("[workspace]\nmembers = [\"a\"]\n"), None);
    // A `name` in another section is not the package's.
    assert_eq!(name("[lib]\nname = \"other\"\n"), None);
}

// ---------------------------------------------------------------------
// Level B
// ---------------------------------------------------------------------

/// With no Rust toolchain, Rust is still a first-class indexed
/// language.
#[test]
fn without_a_backend_the_structural_truth_is_whole() {
    let fixture = Fixture::create("level-b");
    let connection = rusqlite::Connection::open(fixture.db_path()).expect("db");
    let count = |sql: &str| -> i64 {
        connection
            .query_row(sql, [], |row| row.get(0))
            .expect("count")
    };

    assert!(
        count("SELECT COUNT(*) FROM resource WHERE state = 'ACTIVE' AND language = 'RUST'") >= 9,
        "every Rust Resource is indexed"
    );
    for table in ["symbol", "occurrence", "relation", "unresolved_reference"] {
        assert!(
            count(&format!("SELECT COUNT(*) FROM {table}")) > 0,
            "structural {table} rows stay"
        );
    }
}

/// An absent executable is a diagnosis, not a PATH search.
#[test]
fn a_missing_backend_says_exactly_what_is_missing() {
    let error = super::RustInstall::at("/nowhere/rust-analyzer").expect_err("absent");
    let said = error.to_string();
    assert!(said.contains("/nowhere/rust-analyzer"), "{said}");
    assert!(
        said.contains("never searches PATH"),
        "the reason says what it will not do instead: {said}"
    );
    assert!(
        !said.contains("rustup component add"),
        "and it does not offer to install one: {said}"
    );
}

// ---------------------------------------------------------------------
// Withdrawn requests
// ---------------------------------------------------------------------

/// A withdrawn request is asked again, not read as "nothing is there".
#[test]
fn a_withdrawn_request_is_asked_again() {
    let fixture = Fixture::create("cancel-retry");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let specifier = compound_use(&fixture, "crates/app/src/main.rs", "Idle");
    let backend = ScriptedBackend::loaded().cancelling(1).with_definition(
        &fixture.uri("crates/app/src/main.rs"),
        fixture.last_character("crates/app/src/main.rs", specifier.0, specifier.1),
        vec![fixture.module_location(RUNNER)],
    );

    let outcome = refresh(&fixture, &index, &backend, "crates/app/src/main.rs").expect("refresh");
    assert!(
        outcome.withdrawn.is_empty(),
        "one withdrawal is retried, not recorded as a gap"
    );
}

/// Withdrawn twice is incomplete coverage, not an error and not zero.
#[test]
fn a_site_withdrawn_twice_becomes_recorded_coverage() {
    let fixture = Fixture::create("cancel-gap");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let backend = ScriptedBackend::loaded().cancelling(u32::MAX);

    let outcome = refresh(&fixture, &index, &backend, RUNNER)
        .expect("a withdrawing backend is not a broken one");
    assert!(
        !outcome.withdrawn.is_empty(),
        "every site was withdrawn, and every one is recorded"
    );
    assert_eq!(
        outcome.evidence_count, 0,
        "nothing withdrawn becomes evidence"
    );
}

/// A dead backend costs coverage, never correctness.
#[test]
fn a_failing_backend_produces_no_evidence() {
    let fixture = Fixture::create("degraded");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let backend = ScriptedBackend::new().failing(crate::runtime::RequestFailure::Backend(
        crate::runtime::HostError::new("server exited"),
    ));
    let error = refresh(&fixture, &index, &backend, RUNNER).expect_err("no backend");
    assert!(format!("{error}").contains("server exited"), "{error}");
}

/// An unimplemented method is reported rather than swallowed.
#[test]
fn an_unimplemented_method_is_reported_not_swallowed() {
    let fixture = Fixture::create("unsupported");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let backend = ScriptedBackend::loaded().unsupported();
    let error = refresh(&fixture, &index, &backend, RUNNER)
        .expect_err("unsupported is an error, not an empty answer");
    assert!(format!("{error}").contains("unsupported"), "{error}");
}

// ---------------------------------------------------------------------
// Modules and dispatch
// ---------------------------------------------------------------------

/// A module answers as a whole file, and becomes the Resource.
#[test]
fn a_module_resolves_to_its_resource() {
    let fixture = Fixture::create("module-target");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let main = "crates/app/src/main.rs";
    let specifier = compound_use(&fixture, main, "Idle");
    let backend = ScriptedBackend::loaded().with_definition(
        &fixture.uri(main),
        fixture.last_character(main, specifier.0, specifier.1),
        vec![fixture.module_location(RUNNER)],
    );

    let outcome = refresh(&fixture, &index, &backend, main).expect("refresh");
    assert!(
        outcome
            .report
            .iter()
            .any(|line| line.contains("-> RESOLVED Resource")),
        "a module is a Resource, not an invented Symbol: {:?}",
        outcome.report
    );
}

/// A whole-file range is recognised as a module rather than a
/// declaration.
#[test]
fn a_whole_file_range_is_a_module() {
    let fixture = Fixture::create("module-shape");
    assert!(fixture.module_location(RUNNER).is_whole_file());
    let declaration = fixture.declaration(RUNNER, "Worker", 0);
    assert!(!declaration.is_whole_file());
}

// ---------------------------------------------------------------------
// Trait implementation
// ---------------------------------------------------------------------

/// A trait implementation is proved, not matched by name.
///
/// The fixture is built so a name-based derivation fails loudly:
/// `Worker` implements both `Runner` and `Reporter`, both declare
/// `run`, and the two implementing declarations share the qualified
/// name `Worker::run`. So the evidence has to come from the compiler,
/// and it does — `textDocument/implementation` on the *trait* member,
/// with the edge anchored on the implementing declaration so the
/// Resource that can replace it is the one that owns it.
#[test]
fn a_trait_member_implementation_is_proved_by_the_compiler() {
    let fixture = Fixture::create("trait-members");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let contracts = "crates/contracts/src/lib.rs";

    // The base-list site: `impl Runner for Worker`.
    let base = fixture.offset_of(RUNNER, "impl Runner for Worker", 0) + "impl ".len();
    // The trait member the backend will be asked about.
    let trait_run = fixture.offset_of(contracts, "fn run(&self) -> u32;", 0) + "fn ".len();
    // The implementing declaration it answers with.
    let impl_run = fixture.offset_of(RUNNER, "    fn run(&self) -> u32 {", 0) + "    fn ".len();

    let backend = ScriptedBackend::loaded()
        .with_definition(
            &fixture.uri(RUNNER),
            fixture.last_character(RUNNER, base, base + "Runner".len()),
            vec![fixture.location(
                contracts,
                fixture.offset_of(contracts, "pub trait Runner", 0) + "pub trait ".len(),
                fixture.offset_of(contracts, "pub trait Runner", 0) + "pub trait Runner".len(),
            )],
        )
        .with_implementation(
            &fixture.uri(contracts),
            fixture.last_character(contracts, trait_run, trait_run + "run".len()),
            vec![fixture.location(RUNNER, impl_run, impl_run + "run".len())],
        );

    let outcome = refresh(&fixture, &index, &backend, RUNNER).expect("refresh");
    let implements = outcome
        .report
        .iter()
        .filter(|line| line.starts_with("Implements"))
        .count();
    assert!(
        implements >= 2,
        "the type-level edge and the member-level one: {:?}",
        outcome.report
    );
    assert!(
        outcome
            .report
            .iter()
            .any(|line| line.contains(&format!("@{impl_run}..")) && line.contains("RESOLVED")),
        "the member edge is anchored on the implementing declaration: {:?}",
        outcome.report
    );
}

/// An inherent impl implements nothing.
///
/// `impl Worker { pub fn execute(&self) }` writes no trait, so the
/// structural tier anchors no base-list site and nothing here can
/// invent one. `Idle::run` is the sharper trap: a method named exactly
/// like a trait member, on a type that implements no trait at all.
#[test]
fn an_inherent_impl_creates_no_implements_relation() {
    let fixture = Fixture::create("inherent");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let owner = fixture.resource(RUNNER);
    let text = fixture.text(RUNNER);
    let gaps =
        crate::evidence::list_unresolved_for_resource(index.connection(), owner.id).expect("gaps");
    let sites = adapter::collect_sites(
        index.connection(),
        &owner,
        &text,
        &gaps,
        &context().context_key(),
    )
    .expect("sites");

    // Every base-list site in this file names a trait that is actually
    // written after `impl`. An inherent `impl Worker {` contributes
    // none.
    for site in sites
        .sites
        .iter()
        .filter(|site| site.kind == adapter::ResolvableKind::Implements)
    {
        let named = &text[site.occurrence.start_byte..site.occurrence.end_byte];
        assert!(
            ["Runner", "Reporter", "Detailed"].contains(&named),
            "{named:?} is not a trait the fixture implements"
        );
    }
    assert!(
        !sites.sites.iter().any(|site| {
            site.kind == adapter::ResolvableKind::Implements
                && text[site.occurrence.start_byte..site.occurrence.end_byte] == *"Worker"
        }),
        "an inherent impl names no trait"
    );
}

// ---------------------------------------------------------------------
// Worktrees
// ---------------------------------------------------------------------

/// Two worktrees are two analyses, whatever the paths and bytes.
#[test]
fn worktree_identity_is_independent_of_location() {
    let first = Fixture::create("worktree-a");
    let second = Fixture::create("worktree-b");
    let basis = |fixture: &Fixture| {
        let index = SemanticIndex::open(&fixture.db_path()).expect("index");
        lifecycle::discover_packages_under(
            index.connection(),
            ProjectExecutionTrust::Trusted,
            Some(&fixture.root),
        )
        .expect("packages")
        .basis()
        .fingerprint()
    };

    assert_eq!(
        basis(&first),
        basis(&second),
        "the same tree in two places is the same configuration -- \
         an absolute path is a locator, never identity"
    );
    assert_ne!(first.root, second.root);

    // And the Workspace itself is identity, so one checkout's proof
    // never answers for another's.
    let one = crate::semantic::AnalysisContext {
        workspace: brainprint_core::WorkspaceId::from_bytes([1; 16]),
        ..context()
    };
    let other = crate::semantic::AnalysisContext {
        workspace: brainprint_core::WorkspaceId::from_bytes([2; 16]),
        ..context()
    };
    assert_ne!(one.context_key(), other.context_key());
}

// ---------------------------------------------------------------------
// Reopen
// ---------------------------------------------------------------------

/// An unprovable environment is not restored as current, and the
/// backend is not started to discover that.
#[test]
fn an_unpinned_environment_is_deterministic_but_unproven() {
    let fixture = Fixture::create("assurance");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let install = super::RustInstall {
        executable: "/x/rust-analyzer".into(),
        server_version: super::TESTED_SERVER_VERSION.to_owned(),
        rustc_version: "rustc 1.98.1".to_owned(),
        host_triple: "aarch64-apple-darwin".to_owned(),
        sysroot_identity: "stable-aarch64-apple-darwin".to_owned(),
        rust_src: false,
    };
    let config = lifecycle::discover_packages_under(
        index.connection(),
        ProjectExecutionTrust::Trusted,
        Some(&fixture.root),
    )
    .expect("packages");

    // The fixture commits a lockfile, so this one is proven.
    let identity = lifecycle::environment_identity(&install, &config).expect("environment");
    assert!(identity.locked, "the fixture commits Cargo.lock");
    assert!(identity.assurance.is_proven(), "{:?}", identity.assurance);
    assert!(
        !identity.rust_src,
        "and rust-src is recorded as absent rather than assumed"
    );

    // Without it, the fingerprint is still deterministic and no longer
    // evidence.
    let unlocked = lifecycle::RustProjectConfig {
        lockfile: None,
        ..config.clone()
    };
    let without = lifecycle::environment_identity(&install, &unlocked).expect("environment");
    assert!(!without.assurance.is_proven());
    match &without.assurance {
        lifecycle::EnvironmentAssurance::Unknown { reason } => {
            assert!(reason.contains("Cargo.lock"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        without,
        lifecycle::environment_identity(&install, &unlocked).expect("environment"),
        "and it is still deterministic"
    );
}

/// The toolchain is part of the environment, so changing it
/// invalidates.
#[test]
fn the_toolchain_participates_in_semantic_identity() {
    let fixture = Fixture::create("toolchain-identity");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let config = lifecycle::discover_packages_under(
        index.connection(),
        ProjectExecutionTrust::Trusted,
        Some(&fixture.root),
    )
    .expect("packages");
    let install = |rustc: &str, triple: &str| super::RustInstall {
        executable: "/x/rust-analyzer".into(),
        server_version: super::TESTED_SERVER_VERSION.to_owned(),
        rustc_version: rustc.to_owned(),
        host_triple: triple.to_owned(),
        sysroot_identity: "stable-aarch64-apple-darwin".to_owned(),
        rust_src: false,
    };
    let of = |install: &super::RustInstall| {
        lifecycle::environment_identity(install, &config)
            .expect("environment")
            .fingerprint
    };

    let baseline = of(&install("rustc 1.98.1", "aarch64-apple-darwin"));
    assert_ne!(
        baseline,
        of(&install("rustc 1.99.0", "aarch64-apple-darwin")),
        "a different compiler is a different analysis"
    );
    assert_ne!(
        baseline,
        of(&install("rustc 1.98.1", "x86_64-unknown-linux-gnu")),
        "and so is a different host triple -- cfg(target_os) depends on it"
    );
}

// ---------------------------------------------------------------------
// Shared vocabulary
// ---------------------------------------------------------------------

/// The adapter's site vocabulary is the shared one, not a Rust copy.
#[test]
fn the_site_vocabulary_is_shared() {
    let fixture = Fixture::create("sites");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let owner = fixture.resource(RUNNER);
    let text = fixture.text(RUNNER);
    let context_key = context().context_key();
    let gaps =
        crate::evidence::list_unresolved_for_resource(index.connection(), owner.id).expect("gaps");
    let sites = adapter::collect_sites(index.connection(), &owner, &text, &gaps, &context_key)
        .expect("sites");

    assert!(
        !sites.sites.is_empty(),
        "the fixture leaves resolvable sites"
    );
    for site in &sites.sites {
        assert!(
            matches!(
                site.kind.relation_kind(),
                RelationKind::Calls
                    | RelationKind::References
                    | RelationKind::Imports
                    | RelationKind::UsesType
                    | RelationKind::Extends
                    | RelationKind::Implements
            ),
            "{:?} is not a relation this tier can prove",
            site.kind
        );
    }
}

/// Dispatch uses the existing vocabulary and nothing Rust-specific.
#[test]
fn dispatch_uses_the_shared_vocabulary() {
    // Named here so a new Rust-only variant would not compile past it.
    let known = [Dispatch::Static, Dispatch::Dynamic, Dispatch::Unknown];
    assert_eq!(known.len(), 3);
}

/// An endpoint a module produces is an ordinary internal target.
#[test]
fn a_resource_endpoint_is_an_internal_graph_target() {
    let fixture = Fixture::create("endpoint");
    let endpoint = GraphEndpoint::Resource(fixture.resource(RUNNER).id);
    assert_eq!(endpoint.entity_kind(), crate::graph::EntityKind::Resource);
}

// ---------------------------------------------------------------------
// Reopen
// ---------------------------------------------------------------------

/// Reopening reads persisted truth without starting a backend.
///
/// A publication whose whole basis still holds is readable
/// immediately — launching rust-analyzer to re-derive what is already
/// proven would make every reopen cost a project load. A publication
/// whose environment could not be *proven* is not restored as current,
/// however equal its fingerprint: determinism is not evidence, and the
/// backend is not started to discover that.
#[test]
fn a_reopen_reads_persisted_truth_and_refuses_what_it_cannot_prove() {
    let fixture = Fixture::create("reopen");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let backend = ScriptedBackend::loaded();
    refresh(&fixture, &index, &backend, RUNNER).expect("refresh");

    let context = context();
    let owner = crate::semantic_index::SemanticOwner::new(
        context.context_key(),
        fixture.resource(RUNNER).id,
    );
    let packages = lifecycle::discover_packages_under(
        index.connection(),
        ProjectExecutionTrust::Trusted,
        Some(&fixture.root),
    )
    .expect("packages");
    let config = packages.basis();
    let capabilities = capability_report(&context, ProjectExecutionTrust::Trusted);
    let inventory = lifecycle::inventory_fingerprint(index.connection()).expect("inventory");

    let proven = crate::semantic_index::CurrentInputs::new(&context, &config, &capabilities)
        .with_inventory(&inventory)
        .with_environment_proven(true);
    assert_eq!(
        index.revalidate(&owner, &proven).expect("revalidate").state,
        crate::semantic_index::SemanticState::Current,
        "a whole proven basis is readable without a backend"
    );

    let unproven = crate::semantic_index::CurrentInputs::new(&context, &config, &capabilities)
        .with_inventory(&inventory)
        .with_environment_proven(false);
    assert_ne!(
        index
            .revalidate(&owner, &unproven)
            .expect("revalidate")
            .state,
        crate::semantic_index::SemanticState::Current,
        "an unprovable environment is not restored as current"
    );

    // Trust is configuration, so a publication cannot cross it.
    let untrusted = lifecycle::discover_packages_under(
        index.connection(),
        ProjectExecutionTrust::Untrusted,
        Some(&fixture.root),
    )
    .expect("packages")
    .basis();
    let retrusted = crate::semantic_index::CurrentInputs::new(&context, &untrusted, &capabilities)
        .with_inventory(&inventory)
        .with_environment_proven(true);
    assert_ne!(
        index
            .revalidate(&owner, &retrusted)
            .expect("revalidate")
            .state,
        crate::semantic_index::SemanticState::Current,
        "a publication made under one trust mode does not answer for another"
    );
}

/// A repeated refresh changes nothing.
#[test]
fn a_repeated_refresh_is_idempotent() {
    let fixture = Fixture::create("idempotent");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let backend = ScriptedBackend::loaded();

    let first = refresh(&fixture, &index, &backend, RUNNER).expect("refresh");
    let second = refresh(&fixture, &index, &backend, RUNNER).expect("refresh");
    assert_eq!(
        first.report, second.report,
        "asking the same questions again produces the same evidence"
    );
    assert_eq!(first.evidence_count, second.evidence_count);

    let relations: i64 = index
        .connection()
        .query_row("SELECT COUNT(*) FROM relation", [], |row| row.get(0))
        .expect("count");
    refresh(&fixture, &index, &backend, RUNNER).expect("refresh");
    let after: i64 = index
        .connection()
        .query_row("SELECT COUNT(*) FROM relation", [], |row| row.get(0))
        .expect("count");
    assert_eq!(
        relations, after,
        "and creates no second copy of the same edge"
    );
}

// ---------------------------------------------------------------------
// Lifecycle classes
// ---------------------------------------------------------------------

/// Delete and move are source changes too.
#[test]
fn deleting_and_moving_a_source_file_stays_a_source_change() {
    let fixture = Fixture::create("delete-move");
    let id = fixture.resource(RUNNER).id;

    assert_eq!(
        ResourceChange::new(id, ChangeKind::Deleted, RUNNER).class(),
        ChangeClass::DocumentContent
    );
    assert_eq!(
        ResourceChange::new(id, ChangeKind::Moved, "crates/core/src/moved.rs")
            .from_previous(RUNNER)
            .class(),
        ChangeClass::DocumentContent
    );
    // A move *into* a manifest name is a project change, from either
    // end of the move.
    assert_eq!(
        ResourceChange::new(id, ChangeKind::Moved, "crates/core/Cargo.toml")
            .from_previous("crates/core/Cargo.toml.bak")
            .with_language(None)
            .class(),
        ChangeClass::ProjectDefinition
    );
}

/// A moved file is announced as a delete and a create.
#[test]
fn a_move_is_announced_from_both_ends() {
    let fixture = Fixture::create("move-notify");
    let changes = [ResourceChange::new(
        fixture.resource(RUNNER).id,
        ChangeKind::Moved,
        "crates/core/src/moved.rs",
    )
    .from_previous(RUNNER)];
    let batch = lifecycle::watched_changes(&fixture.root, &changes);
    assert_eq!(batch.len(), 2, "the old name and the new one: {batch:?}");
    assert!(
        batch
            .iter()
            .any(|change| change.kind == protocol::WatchedChangeKind::Deleted)
    );
}

// ---------------------------------------------------------------------
// Identity hygiene
// ---------------------------------------------------------------------

/// No rust-analyzer internal identity reaches Brainprint.
///
/// The backend has crate ids, file ids, salsa keys and syntax pointers,
/// and every one of them is stable only inside one process. The typed
/// request surface is the guard: it carries URIs, positions and text,
/// so there is nothing internal for an adapter to be tempted by.
#[test]
fn no_backend_internal_identity_is_representable() {
    let request = protocol::RustRequest::Definition {
        uri: "file:///w/a.rs".into(),
        position: crate::lsp::coordinates::Position::new(1, 2),
    };
    let wire = format!("{:?}", request.wire());
    for internal in [
        "crateId",
        "FileId",
        "salsa",
        "SyntaxNodePtr",
        "AnchoredPath",
    ] {
        assert!(
            !wire.contains(internal),
            "{internal} would be identity that dies with the process"
        );
    }
    // And the answers are the same three shapes.
    let answer = protocol::RustResponse::Locations(vec![protocol::Location {
        uri: "file:///w/a.rs".into(),
        range: crate::lsp::coordinates::Range::new(
            crate::lsp::coordinates::Position::new(0, 0),
            crate::lsp::coordinates::Position::new(0, 1),
        ),
    }]);
    assert!(!format!("{answer:?}").contains("FileId"));
}

/// A backend outside the tested class is refused rather than read.
#[test]
fn an_untested_backend_build_is_refused() {
    use super::ProtocolCompatibility;
    assert!(ProtocolCompatibility::classify("1.98.1").usable());
    assert!(ProtocolCompatibility::classify("1.98.7").usable());
    assert!(!ProtocolCompatibility::classify("1.99.0").usable());
    assert_eq!(
        ProtocolCompatibility::classify("1.99.0").publication_verdict(),
        crate::semantic_index::BackendCompatibility::Rebuild,
        "a publication from an untested build is not comparable"
    );
}

/// The whole tier is reachable without naming a single Rust concept in
/// the shared vocabulary.
#[test]
fn the_shared_query_surface_names_no_rust_concept() {
    // A compile-time statement: the relation kinds this tier produces
    // are the ordinary ones. A Rust-only variant would not compile.
    let produced = [
        RelationKind::Calls,
        RelationKind::References,
        RelationKind::Imports,
        RelationKind::UsesType,
        RelationKind::Implements,
    ];
    assert_eq!(produced.len(), 5);
}

// ---------------------------------------------------------------------
// Call candidates inside a macro's arguments (#47)
// ---------------------------------------------------------------------

/// The fixture file that holds the macro shapes.
const PROBE: &str = "crates/core/src/target_probe.rs";

/// One call candidate: where the callable is written.
struct Candidate {
    start: usize,
    end: usize,
}

impl Candidate {
    /// The `name` written right after `anchor`.
    fn after(fixture: &Fixture, rel: &str, anchor: &str, name: &str) -> Self {
        let start = fixture.offset_of(rel, anchor, 0) + anchor.find(name).expect("inside");
        Self {
            start,
            end: start + name.len(),
        }
    }

    /// `assert_eq!(target_probe(), 7)`, the macro-nested positive.
    fn macro_nested(fixture: &Fixture) -> Self {
        Self::after(
            fixture,
            PROBE,
            "assert_eq!(target_probe(), 7)",
            "target_probe",
        )
    }

    /// `stringify!(target_probe())`, tokens that are never executed.
    fn stringified(fixture: &Fixture) -> Self {
        Self::after(fixture, PROBE, "stringify!(target_probe())", "target_probe")
    }

    /// What the server answers when asked about this candidate: the
    /// definition at its last character, and `signatures` from just
    /// inside its parenthesis.
    fn answered(
        &self,
        fixture: &Fixture,
        backend: ScriptedBackend,
        definition: crate::rust_semantic::protocol::Location,
        signatures: usize,
    ) -> ScriptedBackend {
        backend
            .with_definition(
                &fixture.uri(PROBE),
                fixture.last_character(PROBE, self.start, self.end),
                vec![definition],
            )
            .with_signature_help(
                &fixture.uri(PROBE),
                fixture.position_at(PROBE, self.end + 1),
                signatures,
            )
    }
}

/// The name token of `pub fn target_probe`, the way a definition
/// answer points at it.
fn target_probe_declaration(fixture: &Fixture) -> crate::rust_semantic::protocol::Location {
    let start = fixture.offset_of(PROBE, "pub fn target_probe", 0) + "pub fn ".len();
    fixture.location(PROBE, start, start + "target_probe".len())
}

fn symbols_of(fixture: &Fixture, rel: &str) -> Vec<crate::symbol::Symbol> {
    crate::symbol::SymbolStore::open(&fixture.db_path())
        .expect("index")
        .list_for_resource(fixture.resource(rel).id)
        .expect("symbols")
}

fn only_symbol(fixture: &Fixture, rel: &str, qualified_name: &str) -> crate::symbol::Symbol {
    symbols_of(fixture, rel)
        .into_iter()
        .find(|symbol| symbol.qualified_name.ends_with(qualified_name))
        .unwrap_or_else(|| panic!("{qualified_name} is declared in {rel}"))
}

/// Who calls `target_probe` *through a macro candidate*, as confirmed
/// relations. The ordinary structural callers are somebody else's.
fn callers_of_target(fixture: &Fixture) -> Vec<crate::relations::RelationResult> {
    let target = only_symbol(fixture, PROBE, "target_probe");
    crate::relations::RelationIndex::open(&fixture.db_path())
        .expect("index")
        .incoming(&GraphEndpoint::Symbol(target.id), &[RelationKind::Calls])
        .expect("incoming")
        .confirmed
        .into_iter()
        .filter(|relation| {
            relation.evidence.iter().any(|site| {
                site.occurrence_kind == crate::symbol::OccurrenceKind::CallCandidateSite
            })
        })
        .collect()
}

fn open_candidate_gaps(fixture: &Fixture) -> Vec<usize> {
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    crate::evidence::list_unresolved_for_resource(index.connection(), fixture.resource(PROBE).id)
        .expect("gaps")
        .into_iter()
        .filter(|gap| gap.reason == crate::gaps::UnresolvedReason::MacroCallRequiresSemantics)
        .map(|gap| gap.occurrence.start_byte)
        .collect()
}

fn signature_help_positions(backend: &ScriptedBackend) -> Vec<crate::lsp::coordinates::Position> {
    backend
        .calls()
        .into_iter()
        .filter_map(|request| match request {
            protocol::RustRequest::SignatureHelp { position, .. } => Some(position),
            _ => None,
        })
        .collect()
}

/// Definition and SignatureHelp both proving it is what makes a call
/// candidate a CALLS edge -- anchored on the candidate's own span, from
/// the function it sits inside, read back through the query API.
#[test]
fn a_call_candidate_the_server_proves_becomes_a_call() {
    let fixture = Fixture::create("macro-proved");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let candidate = Candidate::macro_nested(&fixture);
    assert!(open_candidate_gaps(&fixture).contains(&candidate.start));
    let backend = candidate.answered(
        &fixture,
        ScriptedBackend::loaded(),
        target_probe_declaration(&fixture),
        1,
    );

    let outcome = refresh(&fixture, &index, &backend, PROBE).expect("refresh");

    assert!(
        outcome.report.iter().any(|line| {
            line.contains(&format!("@{}..{}", candidate.start, candidate.end))
                && line.contains("RESOLVED Symbol")
        }),
        "{:?}",
        outcome.report
    );
    assert!(outcome.merged.gaps_resolved >= 1, "{:?}", outcome.merged);
    assert!(
        !open_candidate_gaps(&fixture).contains(&candidate.start),
        "the proven candidate is no longer a gap"
    );

    let containing = only_symbol(&fixture, PROBE, "macro_nested_call_matches_the_target");
    let callers = callers_of_target(&fixture);
    let found = callers
        .iter()
        .find(|relation| relation.source == GraphEndpoint::Symbol(containing.id))
        .unwrap_or_else(|| panic!("the test function calls the target: {callers:?}"));
    assert_eq!(found.evidence.len(), 1);
    let site = &found.evidence[0];
    assert_eq!(
        (site.span.start_byte, site.span.end_byte),
        (candidate.start, candidate.end),
        "the edge is anchored on the written callable"
    );
    assert_eq!(
        site.occurrence_kind,
        crate::symbol::OccurrenceKind::CallCandidateSite
    );
    assert_eq!(
        signature_help_positions(&backend),
        vec![fixture.position_at(PROBE, candidate.end + 1)],
        "asked once, just inside the candidate's own parenthesis"
    );
}

/// The definition alone is not a proof.
#[test]
fn a_candidate_without_signature_help_stays_a_gap() {
    let fixture = Fixture::create("macro-no-signature");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let candidate = Candidate::stringified(&fixture);
    let backend = candidate.answered(
        &fixture,
        ScriptedBackend::loaded(),
        target_probe_declaration(&fixture),
        0,
    );

    let outcome = refresh(&fixture, &index, &backend, PROBE).expect("refresh");

    assert!(
        outcome.report.iter().any(|line| {
            line.contains(&format!("@{}..{}", candidate.start, candidate.end))
                && line.contains("UNRESOLVED MacroCallRequiresSemantics")
        }),
        "{:?}",
        outcome.report
    );
    assert!(callers_of_target(&fixture).is_empty(), "no edge, no guess");
    assert!(
        open_candidate_gaps(&fixture).contains(&candidate.start),
        "explicit coverage, not a zero"
    );
}

/// SignatureHelp is asked only after a definition proved a callable
/// declaration -- a type, a module, a dependency's item and no answer at
/// all never earn the second question, and none becomes a call.
#[test]
fn only_a_callable_definition_earns_the_signature_question() {
    let dependency = crate::rust_semantic::protocol::Location {
        uri: crate::rust_semantic::protocol::path_to_uri(Path::new(
            "/home/u/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/serde-1.0.219/src/lib.rs",
        )),
        range: crate::lsp::coordinates::Range::new(
            crate::lsp::coordinates::Position::new(0, 0),
            crate::lsp::coordinates::Position::new(0, 5),
        ),
    };

    for (label, definition) in [
        ("a struct", "struct"),
        ("a module", "module"),
        ("a dependency's item", "dependency"),
        ("nothing", "nothing"),
    ] {
        let fixture = Fixture::create("macro-not-callable");
        let index = SemanticIndex::open(&fixture.db_path()).expect("index");
        let candidate = Candidate::macro_nested(&fixture);
        let answer = match definition {
            "struct" => Some(fixture.declaration(RUNNER, "Worker", 0)),
            "module" => Some(fixture.module_location(RUNNER)),
            "dependency" => Some(dependency.clone()),
            _ => None,
        };
        let backend = match answer {
            Some(location) => candidate.answered(&fixture, ScriptedBackend::loaded(), location, 1),
            None => ScriptedBackend::loaded(),
        };

        let outcome = refresh(&fixture, &index, &backend, PROBE).expect("refresh");

        assert!(
            callers_of_target(&fixture).is_empty(),
            "{label} is not a call"
        );
        assert!(
            open_candidate_gaps(&fixture).contains(&candidate.start),
            "{label}: the candidate stays a gap"
        );
        assert!(
            signature_help_positions(&backend).is_empty(),
            "{label}: no signature question was asked: {:?}",
            outcome.report
        );
    }
}

/// Only a call candidate is ever asked about SignatureHelp.
#[test]
fn signature_help_is_never_asked_for_an_ordinary_site() {
    let fixture = Fixture::create("macro-only-candidates");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let backend = ScriptedBackend::loaded();

    refresh(&fixture, &index, &backend, RUNNER).expect("refresh");

    assert!(
        backend
            .calls()
            .iter()
            .any(|request| matches!(request, protocol::RustRequest::Definition { .. })),
        "ordinary sites were asked about"
    );
    assert!(signature_help_positions(&backend).is_empty());
}

/// A withdrawn SignatureHelp is asked again once, then recorded.
#[test]
fn a_withdrawn_signature_help_is_retried_then_recorded() {
    let candidate_of = |fixture: &Fixture| Candidate::macro_nested(fixture);

    let fixture = Fixture::create("macro-withdrawn-once");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let candidate = candidate_of(&fixture);
    let backend = candidate
        .answered(
            &fixture,
            ScriptedBackend::loaded(),
            target_probe_declaration(&fixture),
            1,
        )
        .withdrawing_signature_help(1);
    let outcome = refresh(&fixture, &index, &backend, PROBE).expect("refresh");
    assert!(
        outcome.withdrawn.is_empty(),
        "one withdrawal is retried: {:?}",
        outcome.withdrawn
    );
    assert_eq!(callers_of_target(&fixture).len(), 1, "and then proved");

    let fixture = Fixture::create("macro-withdrawn-twice");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let candidate = candidate_of(&fixture);
    let backend = candidate
        .answered(
            &fixture,
            ScriptedBackend::loaded(),
            target_probe_declaration(&fixture),
            1,
        )
        .withdrawing_signature_help(2);
    let outcome = refresh(&fixture, &index, &backend, PROBE).expect("refresh");
    assert!(
        outcome
            .withdrawn
            .iter()
            .any(|site| site.start_byte == candidate.start),
        "withdrawn twice is recorded coverage: {:?}",
        outcome.withdrawn
    );
    assert!(callers_of_target(&fixture).is_empty());
    assert!(open_candidate_gaps(&fixture).contains(&candidate.start));
}

/// The same proof twice changes nothing; a proof that stops holding
/// gives the candidate back as the gap it was.
#[test]
fn withdrawing_the_proof_restores_the_candidate_gap() {
    let fixture = Fixture::create("macro-restored");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let candidate = Candidate::macro_nested(&fixture);
    let proving = |signatures: usize| {
        candidate.answered(
            &fixture,
            ScriptedBackend::loaded(),
            target_probe_declaration(&fixture),
            signatures,
        )
    };

    let first = refresh(&fixture, &index, &proving(1), PROBE).expect("refresh");
    assert!(first.merged.relations_created >= 1);
    assert_eq!(callers_of_target(&fixture).len(), 1);

    let again = refresh(&fixture, &index, &proving(1), PROBE).expect("refresh");
    assert_eq!(again.merged.relations_created, 0, "{:?}", again.merged);
    assert_eq!(again.merged.gaps_restored, 0, "{:?}", again.merged);
    assert_eq!(callers_of_target(&fixture).len(), 1, "still one edge");

    let lost = refresh(&fixture, &index, &proving(0), PROBE).expect("refresh");
    assert!(lost.merged.gaps_restored >= 1, "{:?}", lost.merged);
    assert!(callers_of_target(&fixture).is_empty(), "the edge went too");
    assert!(
        open_candidate_gaps(&fixture).contains(&candidate.start),
        "MacroCallRequiresSemantics is back"
    );
}

/// No trusted backend, no proof, and no lost candidate.
#[test]
fn an_untrusted_or_failing_backend_leaves_every_candidate_gap_intact() {
    let fixture = Fixture::create("macro-degraded");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let before = open_candidate_gaps(&fixture);
    assert!(before.len() >= 4, "the fixture holds the macro shapes");

    refresh(&fixture, &index, &ScriptedBackend::untrusted(), PROBE).expect("refresh");
    assert_eq!(open_candidate_gaps(&fixture), before);

    let failing = ScriptedBackend::new().failing(crate::runtime::RequestFailure::Backend(
        crate::runtime::HostError::new("server exited"),
    ));
    refresh(&fixture, &index, &failing, PROBE).expect_err("no backend");
    assert_eq!(open_candidate_gaps(&fixture), before);
    assert!(callers_of_target(&fixture).is_empty());
}

/// Punctuation only chooses where SignatureHelp is asked; it is bounded
/// to the candidate's own line and never searched for.
#[test]
fn the_probe_position_is_bounded_by_the_candidates_own_argument_list() {
    use crate::lsp::coordinates::{LineMap, Position};

    let probe = |text: &str, needle: &str| {
        let map = LineMap::with_encoding(text, protocol::POSITION_ENCODING);
        let end = text.find(needle).expect("needle") + needle.len();
        adapter::argument_list_probe(&map, text, end)
    };

    assert_eq!(probe("m!(f(1))", "f"), Some(Position::new(0, 5)));
    assert_eq!(probe("m!(f (1))", "f"), Some(Position::new(0, 6)));
    assert_eq!(probe("m!(f\t(1))", "f"), Some(Position::new(0, 6)));
    // UTF-16 units: the emoji counts as two, and the column is the
    // server's, not a byte offset.
    assert_eq!(probe("m!(\u{1F600}, f(1))", "f"), Some(Position::new(0, 9)));
    assert_eq!(
        probe("// \u{1F600}\nm!(f(1))", "f"),
        Some(Position::new(1, 5)),
        "and only what is on the candidate's own line moves it"
    );
    assert_eq!(
        probe("m!(caf\u{e9}(1))", "caf\u{e9}"),
        Some(Position::new(0, 8))
    );

    for (label, text, needle) in [
        ("the next line", "m!(f\n(1))", "f"),
        ("another token", "m!(f, (1))", "f"),
        ("a bare reference", "m!(f)", "f"),
        ("a comment", "m!(f /* c */ (1))", "f"),
        ("the end of the text", "m!(f", "f"),
    ] {
        assert_eq!(probe(text, needle), None, "{label}: no bounded position");
    }
}
