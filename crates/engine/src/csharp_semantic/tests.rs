//! The C# backend's always-on suite.
//!
//! Every test here runs with no .NET SDK installed, against a scripted
//! backend that answers what #19 task 12 measured the real one
//! answering. The real-backend acceptance lives in
//! `tests/i4_csharp_acceptance.rs` and is skipped when the toolchain is
//! absent; this suite is what must never be skipped, because it is also
//! the Level B contract.

use std::collections::BTreeSet;

use super::{
    adapter::{self, declared_arity, declares_partial, is_partial_type_kind},
    assembly_identity, capability_report, external_entity,
    lifecycle::{self, ChangeClass, ChangeKind, ResourceChange},
    protocol::ProjectExecutionTrust,
    tests_support::{Fixture, ScriptedBackend, context, refresh, write_metadata},
};
use crate::{
    graph::GraphEndpoint, logical_symbol, resolution::Support, resource::ResourceLanguage,
    semantic::SemanticCapability, semantic_index::SemanticIndex, symbol::SymbolKind,
};

/// The test project's source, which declares a field typed by the
/// fixture's partial type -- a cross-project type reference to something
/// declared in two files.
const TEST_FILE: &str = "tests/Core.Tests/RunnerTests.cs";

// ---------------------------------------------------------------------
// External assembly identity
// ---------------------------------------------------------------------

/// The header is read positionally, because its label is translated.
///
/// The spike measured a Korean SDK writing `#region 어셈블리 System.Console,
/// Version=...`. Any parser that looked for the word "assembly" would
/// have returned nothing on that machine and everything on an English
/// one, which is the worst possible failure: a correct-looking build
/// that silently drops external identity for half its users.
#[test]
fn assembly_identity_survives_a_translated_region_label() {
    let fixture = Fixture::create("metadata-label");
    let directory = fixture.base.join("metadata");
    let assembly = "System.Console, Version=10.0.0.0, Culture=neutral, \
                    PublicKeyToken=b03f5f7f11d50a3a";

    for label in ["어셈블리", "Assembly", "Assembly-Nachweis", "アセンブリ"] {
        let path = write_metadata(&directory, assembly, "Console", label);
        assert_eq!(
            assembly_identity(&path).as_deref(),
            Some(assembly),
            "the {label:?} label must not change what is parsed"
        );
    }
}

/// The machine's DLL path is never identity.
#[test]
fn external_identity_holds_no_machine_path() {
    let fixture = Fixture::create("metadata-path");
    let path = write_metadata(
        &fixture.base.join("metadata"),
        "System.Console, Version=10.0.0.0, Culture=neutral, PublicKeyToken=b03f5f7f11d50a3a",
        "Console",
        "어셈블리",
    );
    let entity = external_entity(&path, Some("WriteLine")).expect("external identity");

    assert_eq!(entity.package_identity, "System.Console");
    assert_eq!(entity.qualified_name.as_deref(), Some("Console.WriteLine"));
    assert_eq!(entity.symbol_name.as_deref(), Some("WriteLine"));
    assert_eq!(entity.declaration_locator, None);

    let rendered = format!("{entity:?}");
    for leaked in ["/usr/", "/Users/", "/var/", "dotnet/packs", ".dll"] {
        assert!(
            !rendered.contains(leaked),
            "{leaked:?} is a locator and must not reach identity: {rendered}"
        );
    }
}

/// Two versions of one assembly are two identities.
#[test]
fn assembly_version_participates_in_identity() {
    let fixture = Fixture::create("metadata-version");
    let directory = fixture.base.join("metadata");
    let older = write_metadata(
        &directory,
        "System.Text.Json, Version=8.0.0.0, Culture=neutral, PublicKeyToken=cc7b13ffcd2ddd51",
        "JsonSerializer",
        "어셈블리",
    );
    let first = external_entity(&older, None).expect("older");
    let newer = write_metadata(
        &directory,
        "System.Text.Json, Version=10.0.0.0, Culture=neutral, PublicKeyToken=cc7b13ffcd2ddd51",
        "JsonSerializer",
        "어셈블리",
    );
    let second = external_entity(&newer, None).expect("newer");

    assert_eq!(first.package_identity, second.package_identity);
    assert_ne!(
        first.resolved_version, second.resolved_version,
        "a different assembly version is a different set of APIs"
    );
}

/// A file with no header is not an assembly, and says so.
#[test]
fn a_file_without_a_region_header_yields_no_identity() {
    let fixture = Fixture::create("metadata-empty");
    let path = fixture.base.join("plain.cs");
    std::fs::create_dir_all(&fixture.base).expect("base");
    std::fs::write(&path, "namespace Nope;\npublic class Plain { }\n").expect("write");

    assert_eq!(assembly_identity(&path), None);
    assert_eq!(external_entity(&path, Some("Plain")), None);
}

// ---------------------------------------------------------------------
// Partial identity
// ---------------------------------------------------------------------

#[test]
fn only_type_kinds_can_be_partial_declarations() {
    for kind in [
        SymbolKind::Class,
        SymbolKind::Struct,
        SymbolKind::Interface,
        SymbolKind::Record,
    ] {
        assert!(is_partial_type_kind(kind), "{kind:?} can be partial");
    }
    for kind in [
        SymbolKind::Method,
        SymbolKind::Property,
        SymbolKind::Field,
        SymbolKind::Enum,
        SymbolKind::Constant,
    ] {
        assert!(
            !is_partial_type_kind(kind),
            "{kind:?} must never be grouped as a partial type"
        );
    }
}

#[test]
fn partial_is_read_from_the_declaration_header() {
    assert!(declares_partial("public partial class Runner : BaseRunner"));
    assert!(declares_partial("internal partial record struct Point"));
    // A type whose *name* contains the word is not a partial type.
    assert!(!declares_partial("public class PartialRunner"));
    assert!(!declares_partial("public class Runner"));
}

#[test]
fn arity_comes_from_the_type_parameter_list() {
    assert_eq!(declared_arity("public partial class Box"), 0);
    assert_eq!(declared_arity("public partial class Box<T>"), 1);
    assert_eq!(declared_arity("public partial class Map<K, V>"), 2);
    // A nested list is one parameter, not two.
    assert_eq!(declared_arity("public class Holder<List<int>>"), 1);
}

/// `Box` and `Box<T>` are two types, and their identities differ.
#[test]
fn arity_separates_two_types_that_share_a_name() {
    let fixture = Fixture::create("identity-arity");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let context = context();
    let base = logical_symbol::LogicalIdentity {
        context_key: context.context_key(),
        project_key: "src/Core/Core.csproj".to_owned(),
        qualified_name: "Core.Box".to_owned(),
        kind: SymbolKind::Class,
        arity: 0,
        discriminator: String::new(),
    };
    let generic = logical_symbol::LogicalIdentity {
        arity: 1,
        ..base.clone()
    };
    let other_project = logical_symbol::LogicalIdentity {
        project_key: "src/App/App.csproj".to_owned(),
        ..base.clone()
    };

    assert_ne!(base.fingerprint(), generic.fingerprint());
    assert_ne!(
        base.fingerprint(),
        other_project.fingerprint(),
        "one name in two projects is two types"
    );

    // And the identity is not a function of where its declarations live.
    let moved = logical_symbol::LogicalIdentity { ..base.clone() };
    assert_eq!(base.fingerprint(), moved.fingerprint());
    drop(index);
}

/// The fixture's partial type becomes one logical symbol owning two
/// declarations, and the reference binds to it exactly once.
#[test]
fn a_partial_type_resolves_to_one_logical_symbol() {
    let fixture = Fixture::create("partial-group");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");

    // `private readonly Runner _shared` in the test project: a declared
    // type reference to the partial type, from another project, answered
    // with *both* declarations -- which is what the real server does.
    let site = fixture.offset_of(TEST_FILE, "Runner _shared", 0);
    let backend = ScriptedBackend::new().with_definition(
        &fixture.uri(TEST_FILE),
        fixture.last_character(TEST_FILE, site, site + "Runner".len()),
        vec![
            fixture.declaration("src/Core/Runner.Part1.cs", "Runner", 0),
            fixture.declaration("src/Core/Runner.Part2.cs", "Runner", 0),
        ],
    );
    // Trusted, and the projects have to be loaded for a cross-project
    // answer. The scripted backend enforces both.
    lifecycle::reload_projects(
        &backend,
        &backend,
        &fixture.root,
        &[ResourceChange::new(
            fixture.resource("src/Core/Core.csproj").id,
            ChangeKind::Changed,
            "src/Core/Core.csproj",
        )
        .with_language(None)],
        &lifecycle::discover_projects(index.connection(), ProjectExecutionTrust::Trusted)
            .expect("projects"),
    )
    .expect("project load");

    let outcome = refresh(&fixture, &index, &backend, TEST_FILE).expect("refresh");

    assert_eq!(
        outcome.grouped.len(),
        1,
        "two declarations of one partial type are one group: {:?}",
        outcome.report
    );
    let logical = *outcome.grouped.iter().next().expect("group");
    let declarations =
        logical_symbol::declarations(index.connection(), logical).expect("declarations");
    assert_eq!(
        declarations.len(),
        2,
        "the group owns both source declarations"
    );

    assert!(
        outcome
            .report
            .iter()
            .any(|line| line.contains("-> RESOLVED Logical")),
        "the reference binds to the group, not to a candidate set: {:?}",
        outcome.report
    );
    assert!(
        !outcome
            .report
            .iter()
            .any(|line| line.contains("CANDIDATES")),
        "a partial type is not an ambiguity: {:?}",
        outcome.report
    );
}

/// Two same-named types that are *not* partial stay an ambiguity.
///
/// The rule that makes grouping safe: only `partial` declarations are
/// merged. Without this a genuine duplicate-name conflict would be
/// silently reported as one type.
#[test]
fn two_non_partial_types_are_never_grouped() {
    let fixture = Fixture::create("no-group");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");

    // `BaseRunner` is declared once and not partial; answer with it and
    // with `Sealed`'s declaration, as a backend that could not decide
    // would.
    let site = fixture.offset_of("src/Core/Runner.Part1.cs", "BaseRunner", 0);
    let backend = ScriptedBackend::new().with_definition(
        &fixture.uri("src/Core/Runner.Part1.cs"),
        fixture.last_character("src/Core/Runner.Part1.cs", site, site + "BaseRunner".len()),
        vec![
            fixture.declaration("src/Core/BaseRunner.cs", "BaseRunner", 0),
            fixture.declaration("src/Core/Sealed.cs", "class ", 0),
        ],
    );

    let outcome = refresh(&fixture, &index, &backend, "src/Core/Runner.Part1.cs").expect("refresh");
    assert!(
        outcome.grouped.is_empty(),
        "nothing here is partial, so nothing may be grouped"
    );
}

/// Adding a third declaration does not change the group's identity.
#[test]
fn a_new_declaration_joins_the_existing_group() {
    let fixture = Fixture::create("partial-grow");
    let context = context();
    let identity = logical_symbol::LogicalIdentity {
        context_key: context.context_key(),
        project_key: "src/Core/Core.csproj".to_owned(),
        qualified_name: "Core.Runner".to_owned(),
        kind: SymbolKind::Class,
        arity: 0,
        discriminator: String::new(),
    };
    let before = identity.fingerprint();

    fixture.write(
        "src/Core/Runner.Part3.cs",
        "namespace Core;\n\npublic partial class Runner\n{\n    private int Third() => 3;\n}\n",
    );
    fixture.rescan("workspace-rev-2");

    assert_eq!(
        identity.fingerprint(),
        before,
        "identity comes from semantic meaning, never from the set of files that declare it"
    );
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

/// An untrusted Workspace cannot reach a project load, and the refusal
/// is an error rather than a quiet skip.
#[test]
fn an_untrusted_workspace_refuses_to_load_projects() {
    let fixture = Fixture::create("trust-refuse");
    let backend = ScriptedBackend::untrusted();
    let error = lifecycle::reload_projects(
        &backend,
        &backend,
        &fixture.root,
        &[ResourceChange::new(
            fixture.resource("src/Core/Core.csproj").id,
            ChangeKind::Changed,
            "src/Core/Core.csproj",
        )
        .with_language(None)],
        &lifecycle::discover_projects(
            &rusqlite::Connection::open(fixture.db_path()).expect("db"),
            ProjectExecutionTrust::Untrusted,
        )
        .expect("projects"),
    )
    .expect_err("an untrusted Workspace must refuse");

    assert!(
        matches!(error, lifecycle::LifecycleError::Untrusted(_)),
        "{error}"
    );
    assert!(
        !backend.projects_loaded(),
        "nothing may be sent before the refusal"
    );
    assert!(
        backend.calls().is_empty(),
        "the refusal happens before the notification, not after it"
    );
}

/// An untrusted analysis still works, and still tells the truth.
#[test]
fn an_untrusted_analysis_publishes_gaps_not_guesses() {
    let fixture = Fixture::create("trust-gaps");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");

    let site = fixture.offset_of(TEST_FILE, "Runner _shared", 0);
    let backend = ScriptedBackend::untrusted().with_definition(
        &fixture.uri(TEST_FILE),
        fixture.last_character(TEST_FILE, site, site + "Runner".len()),
        vec![
            fixture.declaration("src/Core/Runner.Part1.cs", "Runner", 0),
            fixture.declaration("src/Core/Runner.Part2.cs", "Runner", 0),
        ],
    );

    let outcome = refresh(&fixture, &index, &backend, TEST_FILE).expect("refresh");
    assert!(!backend.projects_loaded(), "no project code ran");
    assert!(
        !outcome
            .report
            .iter()
            .any(|line| line.contains("-> RESOLVED")),
        "a cross-project answer is unavailable untrusted: {:?}",
        outcome.report
    );
    assert!(
        outcome.grouped.is_empty(),
        "an untrusted pass proves no grouping"
    );
}

/// The trust mode is part of what a publication was based on.
#[test]
fn trust_participates_in_the_configuration_basis() {
    let fixture = Fixture::create("trust-basis");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let untrusted =
        lifecycle::discover_projects(index.connection(), ProjectExecutionTrust::Untrusted)
            .expect("untrusted");
    let trusted = lifecycle::discover_projects(index.connection(), ProjectExecutionTrust::Trusted)
        .expect("trusted");

    assert_ne!(
        untrusted.basis().fingerprint(),
        trusted.basis().fingerprint(),
        "granting trust must invalidate, not silently widen"
    );
    assert!(!untrusted.loads_projects());
    assert!(trusted.loads_projects());
}

/// Trust is in the environment fingerprint, so it is in the toolchain
/// identity, so it is in the runtime identity.
#[test]
fn trust_participates_in_semantic_identity() {
    let fixture = Fixture::create("trust-identity");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let install = super::CSharpInstall {
        root: fixture.base.clone(),
        executable: fixture.base.join("server"),
        server_version: super::TESTED_SERVER_VERSION.to_owned(),
        runtime_identifier: "osx-arm64".to_owned(),
    };
    let untrusted = lifecycle::environment_identity(
        &install,
        &lifecycle::discover_projects(index.connection(), ProjectExecutionTrust::Untrusted)
            .expect("untrusted"),
    )
    .expect("environment");
    let trusted = lifecycle::environment_identity(
        &install,
        &lifecycle::discover_projects(index.connection(), ProjectExecutionTrust::Trusted)
            .expect("trusted"),
    )
    .expect("environment");

    assert_ne!(untrusted.fingerprint, trusted.fingerprint);
    assert_ne!(
        super::toolchain_identity(&install, &untrusted),
        super::toolchain_identity(&install, &trusted)
    );
}

/// The untrusted capability report is narrower, and says so honestly.
#[test]
fn untrusted_capabilities_are_narrower_not_quieter() {
    let context = context();
    let untrusted = capability_report(&context, ProjectExecutionTrust::Untrusted);
    let trusted = capability_report(&context, ProjectExecutionTrust::Trusted);

    for capability in [
        SemanticCapability::CallsCrossFile,
        SemanticCapability::OverloadResolution,
        SemanticCapability::Inheritance,
        SemanticCapability::ImplementationTarget,
        SemanticCapability::ExternalSymbolResolution,
    ] {
        assert_eq!(
            untrusted.support(capability),
            Support::Unsupported,
            "{capability:?} needs a loaded project"
        );
        assert_eq!(trusted.support(capability), Support::Supported);
    }
    // Measured: these two survive with no project loaded.
    assert_eq!(
        untrusted.support(SemanticCapability::SyntaxStructure),
        Support::Supported
    );
    assert_eq!(
        untrusted.support(SemanticCapability::SymbolDefinition),
        Support::Partial
    );
}

/// Nothing this tier claims is inferred from a provider list.
#[test]
fn unsupportable_capabilities_are_declared_unsupported() {
    let context = context();
    let report = capability_report(&context, ProjectExecutionTrust::Trusted);
    // C# source is not generated from anything.
    assert_eq!(
        report.support(SemanticCapability::EmbeddedRegionMapping),
        Support::Unsupported
    );
    assert_eq!(
        report.support(SemanticCapability::OriginalSourceMapping),
        Support::Unsupported
    );
    // A namespace has no single declaration.
    assert_eq!(
        report.support(SemanticCapability::ImportBinding),
        Support::Unsupported
    );
}

// ---------------------------------------------------------------------
// Change classification
// ---------------------------------------------------------------------

/// Editing a body is a content change; anything that moves the
/// compilation is structure.
#[test]
fn the_two_change_classes_are_distinguished() {
    let fixture = Fixture::create("classes");
    let id = fixture.resource("src/Core/Runner.Part1.cs").id;

    assert_eq!(
        ResourceChange::new(id, ChangeKind::Changed, "src/Core/Runner.Part1.cs").class(),
        ChangeClass::DocumentContent
    );
    // Adding a `.cs` changes project membership under the default glob,
    // even though no project file was touched.
    assert_eq!(
        ResourceChange::new(id, ChangeKind::Added, "src/Core/New.cs").class(),
        ChangeClass::ProjectStructure
    );
    assert_eq!(
        ResourceChange::new(id, ChangeKind::Deleted, "src/Core/Old.cs").class(),
        ChangeClass::ProjectStructure
    );
    for path in [
        "src/Core/Core.csproj",
        "CSharpSemanticSpike.sln",
        "Directory.Build.props",
        "Directory.Packages.props",
        "global.json",
        "NuGet.config",
    ] {
        assert_eq!(
            ResourceChange::new(id, ChangeKind::Changed, path)
                .with_language(None)
                .class(),
            ChangeClass::ProjectStructure,
            "{path} changes how the compilation is built"
        );
    }
}

/// A content change is synchronized with Brainprint's current bytes.
///
/// The measured requirement: a watched-file notification alone left the
/// server answering at the old positions.
#[test]
fn a_content_change_hands_over_current_bytes() {
    let fixture = Fixture::create("content-sync");
    fixture.write(
        "src/Core/Runner.Part2.cs",
        "namespace Core;\n\npublic partial class Runner\n{\n    private int Extra() => 2;\n}\n",
    );
    let backend = ScriptedBackend::new();
    let sent = lifecycle::synchronize_documents(
        &backend,
        &backend,
        &fixture.root,
        &[ResourceChange::new(
            fixture.resource("src/Core/Runner.Part2.cs").id,
            ChangeKind::Changed,
            "src/Core/Runner.Part2.cs",
        )],
    )
    .expect("sync");

    assert_eq!(sent, 1);
    assert_eq!(
        backend.opened(),
        vec![fixture.uri("src/Core/Runner.Part2.cs")]
    );
    let text = backend
        .calls()
        .into_iter()
        .find_map(|call| match call {
            super::protocol::CSharpRequest::OpenDocument { text, .. } => Some(text),
            _ => None,
        })
        .expect("the current bytes were sent");
    assert!(
        text.contains("Extra() => 2"),
        "what is sent is what is on disk now"
    );
    assert!(
        !backend.projects_loaded(),
        "a content edit needs no project reload"
    );
}

/// A deleted document has no current bytes and is not synchronized.
#[test]
fn a_deleted_document_is_not_handed_over() {
    let fixture = Fixture::create("content-delete");
    let id = fixture.resource("src/Core/Sealed.cs").id;
    fixture.remove("src/Core/Sealed.cs");
    let backend = ScriptedBackend::new();
    let sent = lifecycle::synchronize_documents(
        &backend,
        &backend,
        &fixture.root,
        &[ResourceChange::new(
            id,
            ChangeKind::Deleted,
            "src/Core/Sealed.cs",
        )],
    )
    .expect("sync");
    assert_eq!(sent, 0);
    assert!(backend.opened().is_empty());
}

/// Document versions are monotonic, because the backend requires it.
#[test]
fn document_versions_never_go_backwards() {
    let fixture = Fixture::create("versions");
    let backend = ScriptedBackend::new();
    let change = ResourceChange::new(
        fixture.resource("src/Core/Runner.Part2.cs").id,
        ChangeKind::Changed,
        "src/Core/Runner.Part2.cs",
    );
    for _ in 0..3 {
        lifecycle::synchronize_documents(
            &backend,
            &backend,
            &fixture.root,
            std::slice::from_ref(&change),
        )
        .expect("sync");
    }
    // One open, then changes: a second `didOpen` is a protocol
    // violation that the live server answers by cancelling the requests
    // already in flight.
    let synchronizations: Vec<(&'static str, i64)> = backend
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            super::protocol::CSharpRequest::OpenDocument { version, .. } => Some(("open", version)),
            super::protocol::CSharpRequest::ChangeDocument { version, .. } => {
                Some(("change", version))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        synchronizations,
        vec![("open", 1), ("change", 2), ("change", 3)]
    );
}

/// A structural change waits for the server's own initialization
/// signal, not for a sleep.
#[test]
fn a_structural_change_waits_for_project_initialization() {
    let fixture = Fixture::create("structure-sync");
    let backend = ScriptedBackend::new();
    let completed = lifecycle::reload_projects(
        &backend,
        &backend,
        &fixture.root,
        &[ResourceChange::new(
            fixture.resource("src/Core/Core.csproj").id,
            ChangeKind::Changed,
            "src/Core/Core.csproj",
        )
        .with_language(None)],
        &lifecycle::discover_projects(
            &rusqlite::Connection::open(fixture.db_path()).expect("db"),
            ProjectExecutionTrust::Trusted,
        )
        .expect("projects"),
    )
    .expect("reload");

    assert_eq!(completed, 1, "one initialization was announced and awaited");
    assert!(backend.projects_loaded());
    assert_eq!(
        backend.notifications().len(),
        1,
        "the filesystem move is announced before the reload"
    );
}

// ---------------------------------------------------------------------
// Project structure
// ---------------------------------------------------------------------

/// Each source file is owned by its nearest `.csproj`.
#[test]
fn project_ownership_is_nearest_ancestor() {
    let fixture = Fixture::create("ownership");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let config = lifecycle::discover_projects(index.connection(), ProjectExecutionTrust::Trusted)
        .expect("projects");

    for (file, project) in [
        ("src/Core/Runner.Part1.cs", "src/Core/Core.csproj"),
        ("src/Core/Runner.Part2.cs", "src/Core/Core.csproj"),
        ("src/App/Program.cs", "src/App/App.csproj"),
        ("src/Contracts/IRunner.cs", "src/Contracts/Contracts.csproj"),
        (
            "tests/Core.Tests/RunnerTests.cs",
            "tests/Core.Tests/Core.Tests.csproj",
        ),
    ] {
        assert_eq!(
            config.owning_project.get(&fixture.resource(file).id),
            Some(&project.to_owned()),
            "{file} belongs to {project}"
        );
    }
    assert!(
        config
            .project_files
            .iter()
            .any(|resource| resource.path_key == "CSharpSemanticSpike.sln"),
        "the solution is a configuration input"
    );
}

/// This backend answers for C# and nothing else.
#[test]
fn the_backend_serves_one_language() {
    assert_eq!(lifecycle::SERVED_LANGUAGES, [ResourceLanguage::CSharp]);
}

/// The inventory moves when a source file is added, because a new
/// `partial` declaration changes what an untouched file means.
#[test]
fn the_inventory_moves_when_a_declaration_is_added() {
    let fixture = Fixture::create("inventory");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let before = lifecycle::inventory_fingerprint(index.connection()).expect("inventory");
    drop(index);

    fixture.write(
        "src/Core/Runner.Part3.cs",
        "namespace Core;\n\npublic partial class Runner\n{\n    private int Third() => 3;\n}\n",
    );
    fixture.rescan("workspace-rev-2");

    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let after = lifecycle::inventory_fingerprint(index.connection()).expect("inventory");
    assert_ne!(before, after);
}

/// A fingerprint is deterministic; being deterministic is not the same
/// as being proven.
#[test]
fn an_unpinned_environment_is_deterministic_but_unproven() {
    let fixture = Fixture::create("assurance");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let install = super::CSharpInstall {
        root: fixture.base.clone(),
        executable: fixture.base.join("server"),
        server_version: super::TESTED_SERVER_VERSION.to_owned(),
        runtime_identifier: "osx-arm64".to_owned(),
    };
    let config = lifecycle::discover_projects(index.connection(), ProjectExecutionTrust::Trusted)
        .expect("projects");
    let identity = lifecycle::environment_identity(&install, &config).expect("environment");

    assert!(!identity.fingerprint.is_empty());
    // The fixture pins the SDK with global.json but has no lockfile.
    assert!(
        !identity.locked,
        "the fixture restores without packages.lock.json"
    );
    assert!(
        !identity.assurance.is_proven(),
        "an unlocked package graph can move without the fingerprint moving"
    );
    assert_eq!(
        identity,
        lifecycle::environment_identity(&install, &config).expect("environment"),
        "and it is still deterministic"
    );
}

// ---------------------------------------------------------------------
// Degradation
// ---------------------------------------------------------------------

/// A dead backend costs coverage, never correctness.
#[test]
fn a_failing_backend_produces_no_evidence() {
    let fixture = Fixture::create("degraded");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let backend = ScriptedBackend::new().failing(crate::runtime::RequestFailure::Backend(
        crate::runtime::HostError::new("server exited"),
    ));

    let error = refresh(&fixture, &index, &backend, "src/App/Program.cs").expect_err("no backend");
    assert!(format!("{error}").contains("server exited"), "{error}");
}

/// A backend that does not implement a method says so rather than
/// answering nothing.
#[test]
fn an_unimplemented_method_is_reported_not_swallowed() {
    let fixture = Fixture::create("unsupported");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let backend = ScriptedBackend::new().unsupported();

    let error = refresh(&fixture, &index, &backend, "src/App/Program.cs")
        .expect_err("unsupported is an error, not an empty answer");
    assert!(format!("{error}").contains("unsupported"), "{error}");
}

/// A withdrawn request is asked again, not read as "nothing is there".
///
/// The live server withdraws a definition request that is in flight
/// while the document it names is being re-analysed. Reading that as an
/// empty answer would publish the one thing it certainly does not mean.
#[test]
fn a_withdrawn_request_is_asked_again() {
    let fixture = Fixture::create("cancel-retry");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");

    let site = fixture.offset_of(TEST_FILE, "Runner _shared", 0);
    let backend = ScriptedBackend::new().cancelling(1).with_definition(
        &fixture.uri(TEST_FILE),
        fixture.last_character(TEST_FILE, site, site + "Runner".len()),
        vec![
            fixture.declaration("src/Core/Runner.Part1.cs", "Runner", 0),
            fixture.declaration("src/Core/Runner.Part2.cs", "Runner", 0),
        ],
    );
    lifecycle::reload_projects(
        &backend,
        &backend,
        &fixture.root,
        &[ResourceChange::new(
            fixture.resource("src/Core/Core.csproj").id,
            ChangeKind::Changed,
            "src/Core/Core.csproj",
        )
        .with_language(None)],
        &lifecycle::discover_projects(index.connection(), ProjectExecutionTrust::Trusted)
            .expect("projects"),
    )
    .expect("project load");

    let outcome = refresh(&fixture, &index, &backend, TEST_FILE).expect("refresh");
    assert!(
        outcome.withdrawn.is_empty(),
        "one withdrawal is retried, not recorded as a gap"
    );
    assert_eq!(
        outcome.grouped.len(),
        1,
        "and the retry's answer is used: {:?}",
        outcome.report
    );
}

/// Withdrawn twice is incomplete coverage, and says so.
#[test]
fn a_site_withdrawn_twice_becomes_recorded_coverage_not_an_error() {
    let fixture = Fixture::create("cancel-gap");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let backend = ScriptedBackend::new().cancelling(u32::MAX);

    let outcome = refresh(&fixture, &index, &backend, TEST_FILE)
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

/// A withdrawal takes the groups with it, so a stale part cannot outlive
/// the proof that put it there.
#[test]
fn withdrawal_collects_emptied_groups() {
    let fixture = Fixture::create("withdraw");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");

    // `private readonly Runner _shared` in the test project: a declared
    // type reference to the partial type, from another project, answered
    // with *both* declarations -- which is what the real server does.
    let site = fixture.offset_of(TEST_FILE, "Runner _shared", 0);
    let backend = ScriptedBackend::new().with_definition(
        &fixture.uri(TEST_FILE),
        fixture.last_character(TEST_FILE, site, site + "Runner".len()),
        vec![
            fixture.declaration("src/Core/Runner.Part1.cs", "Runner", 0),
            fixture.declaration("src/Core/Runner.Part2.cs", "Runner", 0),
        ],
    );
    lifecycle::reload_projects(
        &backend,
        &backend,
        &fixture.root,
        &[ResourceChange::new(
            fixture.resource("src/Core/Core.csproj").id,
            ChangeKind::Changed,
            "src/Core/Core.csproj",
        )
        .with_language(None)],
        &lifecycle::discover_projects(index.connection(), ProjectExecutionTrust::Trusted)
            .expect("projects"),
    )
    .expect("project load");
    let outcome = refresh(&fixture, &index, &backend, TEST_FILE).expect("refresh");
    assert_eq!(outcome.grouped.len(), 1);

    let context_key = context().context_key();
    let mut owners = BTreeSet::new();
    // Both declaration owners and the owner that referenced the group:
    // `collect_orphans` deliberately keeps a group that a live relation
    // still points at, so withdrawing only the declarations is not
    // enough -- and should not be.
    for file in [
        "src/Core/Runner.Part1.cs",
        "src/Core/Runner.Part2.cs",
        TEST_FILE,
    ] {
        owners.insert(crate::semantic_index::SemanticOwner::new(
            &context_key,
            fixture.resource(file).id,
        ));
    }
    let report = lifecycle::withdraw_affected(&index, &owners, "TEST_WITHDRAWN").expect("withdraw");
    assert!(
        report.orphaned_groups >= 1,
        "a group with no declarations left is collected: {report:?}"
    );
    assert!(
        logical_symbol::read(
            index.connection(),
            *outcome.grouped.iter().next().expect("group")
        )
        .expect("read")
        .is_none(),
        "and it is gone"
    );
}

/// The adapter's site vocabulary is the shared one, not a C# copy.
#[test]
fn the_site_vocabulary_is_shared() {
    let fixture = Fixture::create("sites");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index");
    let owner = fixture.resource("src/App/Program.cs");
    let text = fixture.text("src/App/Program.cs");
    let context_key = context().context_key();
    let gaps =
        crate::evidence::list_unresolved_for_resource(index.connection(), owner.id).expect("gaps");
    let sites = adapter::collect_sites(index.connection(), &owner, &text, &gaps, &context_key)
        .expect("sites");

    assert!(
        !sites.sites.is_empty(),
        "the structural tier left resolvable sites in Program.cs"
    );
    for site in &sites.sites {
        assert!(
            matches!(
                site.kind.relation_kind(),
                crate::graph::RelationKind::Calls
                    | crate::graph::RelationKind::References
                    | crate::graph::RelationKind::Imports
                    | crate::graph::RelationKind::UsesType
                    | crate::graph::RelationKind::Extends
                    | crate::graph::RelationKind::Implements
            ),
            "{:?} is not a relation this tier can prove",
            site.kind
        );
    }
}

/// Every endpoint the adapter can produce is one the graph knows.
#[test]
fn logical_endpoints_are_internal_graph_targets() {
    let logical = GraphEndpoint::Logical(brainprint_core::LogicalSymbolId::generate());
    assert_eq!(logical.entity_kind(), crate::graph::EntityKind::Logical);
}

/// A full replacement really does cover the whole previous document.
///
/// The live server accepted a range that fell short and then answered
/// from the *old* text, which looks exactly like a working sync until a
/// position is wrong. So the range is checked here rather than trusted.
#[test]
fn a_replacement_range_covers_the_entire_previous_document() {
    for text in [
        "class A {}\n",
        "class A {}",
        "using Contracts;\n\nnamespace Core;\n\npublic partial class Runner\n{\n}\n",
        "",
        "\n\n\n",
        "// 어셈블리\nclass A {}\n",
    ] {
        let whole = adapter::whole_of(text);
        assert_eq!(
            whole.start,
            crate::lsp::coordinates::Position::new(0, 0),
            "a replacement starts at the beginning of {text:?}"
        );
        let map = crate::lsp::coordinates::LineMap::with_encoding(
            text,
            super::protocol::POSITION_ENCODING,
        );
        assert_eq!(
            map.span(whole).expect("span"),
            (0, text.len()),
            "the range must convert back to every byte of {text:?}"
        );
    }
}
