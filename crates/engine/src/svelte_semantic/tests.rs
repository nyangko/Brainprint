//! Always-on tests for the Svelte semantic tier.
//!
//! No Node, no `svelte-language-server`, no network. What is checked
//! here is everything that does not need the backend installed: how a
//! location becomes identity, what happens when it cannot, and how the
//! lifecycle behaves when there is no backend at all. The real server's
//! answers are checked by `tests/i4_svelte_acceptance.rs`, which is
//! ignored by default.

use std::collections::BTreeSet;

use super::{
    adapter::{self, MappingFailure, SvelteQueries},
    lifecycle::{self, ChangeKind, EnvironmentAssurance, ResourceChange, SemanticAvailability},
    protocol::{
        FORBIDDEN_SYNC_METHODS, Location, SvelteRequest, SvelteResponse, WatchedChangeKind,
        is_generated_uri,
    },
    tests_support::{Fixture, ScriptedBackend, backend_for_parent, context, refresh},
};
use crate::{
    graph::{GraphEndpoint, RelationKind},
    lsp::coordinates::{Position, Range},
    resource::ResourceLanguage,
    runtime::{HostError, RequestFailure, RuntimeState},
    semantic::{SemanticCapability, SemanticOutcome},
    semantic_index::{SemanticIndex, SemanticOwner, SemanticState},
};

// ---------------------------------------------------------------------
// The original-source contract
// ---------------------------------------------------------------------

#[test]
fn a_template_identifier_resolves_to_the_script_declaration_it_names() {
    let fixture = Fixture::create("template-identifier");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let backend = backend_for_parent(&fixture);

    let outcome = refresh(&fixture, &index, &backend, "src/Parent.svelte").expect("refresh");
    assert!(
        outcome
            .report
            .iter()
            .any(|line| line.starts_with("References") && line.contains("-> RESOLVED Symbol")),
        "{:?}",
        outcome.report
    );

    // And it is a canonical edge from the component to the declaration,
    // anchored on the template span rather than on the script one.
    let parent = fixture.resource("src/Parent.svelte");
    let increment_use = fixture.offset_of("src/Parent.svelte", "increment}", 0);
    let anchored: i64 = index
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM semantic_evidence \
             JOIN occurrence ON occurrence.id = semantic_evidence.occurrence_id \
             WHERE occurrence.start_byte = ?1 AND occurrence.kind = 'REFERENCE_SITE'",
            rusqlite::params![i64::try_from(increment_use).expect("fits")],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(
        anchored, 1,
        "anchored on the template use, in the component"
    );
    assert!(!parent.path_key.is_empty());
}

#[test]
fn a_component_tag_resolves_to_the_component_resource_not_a_generated_class() {
    let fixture = Fixture::create("component-tag");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let backend = backend_for_parent(&fixture);
    refresh(&fixture, &index, &backend, "src/Parent.svelte").expect("refresh");

    // The measured server names a component as its *file*, at a
    // degenerate position. That is the canonical component identity --
    // not a synthetic `svelte2tsx` class with borrowed coordinates.
    let child = fixture.resource("src/lib/Child.svelte");
    let targets: i64 = index
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM relation r \
             JOIN graph_entity te ON te.id = r.target_entity_id \
             JOIN resource res ON res.id = te.resource_id \
             WHERE r.kind = 'REFERENCES' AND res.uid = ?1",
            rusqlite::params![child.id.to_bytes().to_vec()],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(targets, 1, "`<Child …>` references the component Resource");
}

#[test]
fn a_generated_location_is_refused_rather_than_published() {
    let fixture = Fixture::create("generated-refused");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");

    // What a future language-server version might start leaking. It is
    // not source anyone wrote, so it may never become a span.
    let increment_use = fixture.offset_of("src/Parent.svelte", "increment}", 0);
    let at = fixture.last_character(
        "src/Parent.svelte",
        increment_use,
        increment_use + "increment".len(),
    );
    let generated = Location {
        uri: format!("{}.tsx", fixture.uri("src/Parent.svelte")),
        range: Range::new(Position::new(40, 4), Position::new(40, 13)),
    };
    let backend = ScriptedBackend::new().with_definition(
        &fixture.uri("src/Parent.svelte"),
        at,
        vec![generated],
    );

    let outcome = refresh(&fixture, &index, &backend, "src/Parent.svelte").expect("refresh");
    assert!(
        outcome
            .mapping_failures
            .iter()
            .any(|(failure, _)| *failure == MappingFailure::GeneratedLocation),
        "the refusal is reported as incomplete coverage: {:?}",
        outcome.mapping_failures
    );
    assert!(
        outcome
            .report
            .iter()
            .all(|line| !line.contains("-> RESOLVED")),
        "and nothing was published from it: {:?}",
        outcome.report
    );
    // No Resource was created for the generated file either.
    assert!(
        fixture
            .resources()
            .iter()
            .all(|resource| !resource.path_key.ends_with(".svelte.tsx")),
        "generated output is never a project Resource"
    );
}

#[test]
fn a_range_that_does_not_convert_exactly_is_refused_rather_than_clamped() {
    let fixture = Fixture::create("inexact-range");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let increment_use = fixture.offset_of("src/Parent.svelte", "increment}", 0);
    let at = fixture.last_character(
        "src/Parent.svelte",
        increment_use,
        increment_use + "increment".len(),
    );
    let backend = ScriptedBackend::new().with_definition(
        &fixture.uri("src/Parent.svelte"),
        at,
        vec![Location {
            uri: fixture.uri("src/Parent.svelte"),
            range: Range::new(Position::new(9_000, 0), Position::new(9_000, 4)),
        }],
    );

    let outcome = refresh(&fixture, &index, &backend, "src/Parent.svelte").expect("refresh");
    assert!(
        outcome
            .mapping_failures
            .iter()
            .any(|(failure, _)| *failure == MappingFailure::InexactRange),
        "{:?}",
        outcome.mapping_failures
    );
}

#[test]
fn a_component_nobody_synchronized_is_loud_rather_than_empty() {
    // The measured server errors on a document it has never been told
    // about. That is the ideal failure mode and this tier preserves it:
    // a missing synchronization can never arrive as "no findings".
    let fixture = Fixture::create("unsynchronized");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let backend = ScriptedBackend::new().enforcing_synchronization();

    let error = refresh(&fixture, &index, &backend, "src/Parent.svelte")
        .expect_err("an unsynchronized component is not an empty answer");
    assert!(
        matches!(
            error,
            super::SvelteSemanticError::Backend(adapter::AdapterError::Unsynchronized { .. })
        ),
        "{error}"
    );

    // And after the batch that announces it, the same question answers.
    let synchronized = backend_for_parent(&fixture).enforcing_synchronization();
    lifecycle::announce_components(&index, &synchronized, &fixture.root).expect("announce");
    refresh(&fixture, &index, &synchronized, "src/Parent.svelte").expect("now it answers");
}

#[test]
fn the_generated_output_marker_recognises_what_svelte2tsx_writes() {
    assert!(is_generated_uri("file:///w/src/Parent.svelte.tsx"));
    assert!(is_generated_uri("file:///w/x/__sveltets_2_helpers.d.ts"));
    assert!(!is_generated_uri("file:///w/src/Parent.svelte"));
}

// ---------------------------------------------------------------------
// Publication and merge
// ---------------------------------------------------------------------

#[test]
fn a_second_refresh_changes_nothing() {
    let fixture = Fixture::create("idempotent");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let backend = backend_for_parent(&fixture);

    let first = refresh(&fixture, &index, &backend, "src/Parent.svelte").expect("first");
    assert!(first.merged.relations_created > 0);
    let relations: i64 = index
        .connection()
        .query_row("SELECT COUNT(*) FROM relation", [], |row| row.get(0))
        .expect("count");

    let second = refresh(&fixture, &index, &backend, "src/Parent.svelte").expect("second");
    assert_eq!(second.merged.relations_created, 0);
    assert_eq!(second.merged.relations_removed, 0);
    assert_eq!(
        index
            .connection()
            .query_row("SELECT COUNT(*) FROM relation", [], |row| row
                .get::<_, i64>(0))
            .expect("count"),
        relations,
        "asking the same question twice does not double the graph"
    );
}

#[test]
fn one_logical_relation_is_one_row_however_many_tiers_proved_it() {
    let fixture = Fixture::create("no-duplicates");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    refresh(
        &fixture,
        &index,
        &backend_for_parent(&fixture),
        "src/Parent.svelte",
    )
    .expect("refresh");
    assert_eq!(
        index
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM (SELECT kind, source_entity_id, target_entity_id, \
                 COUNT(*) AS n FROM relation \
                 GROUP BY kind, source_entity_id, target_entity_id HAVING n > 1)",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("count"),
        0
    );
}

#[test]
fn withdrawing_a_contribution_leaves_the_structural_truth_alone() {
    let fixture = Fixture::create("withdraw");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    refresh(
        &fixture,
        &index,
        &backend_for_parent(&fixture),
        "src/Parent.svelte",
    )
    .expect("refresh");

    let owner = SemanticOwner::new(
        context().context_key(),
        fixture.resource("src/Parent.svelte").id,
    );
    let occurrences_before: i64 = index
        .connection()
        .query_row("SELECT COUNT(*) FROM occurrence", [], |row| row.get(0))
        .expect("count");

    let report = lifecycle::withdraw_affected(
        &index,
        &BTreeSet::from([owner]),
        crate::semantic_index::SOURCE_MOVED_CODE,
    )
    .expect("withdraw");
    assert!(report.relations_removed > 0, "the semantic edges went");
    assert_eq!(
        index
            .connection()
            .query_row("SELECT COUNT(*) FROM occurrence", [], |row| row
                .get::<_, i64>(0))
            .expect("count"),
        occurrences_before,
        "the component's own structure is untouched -- it was never the backend's"
    );
}

#[test]
fn a_backend_failure_keeps_the_last_valid_publication() {
    let fixture = Fixture::create("failure");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    refresh(
        &fixture,
        &index,
        &backend_for_parent(&fixture),
        "src/Parent.svelte",
    )
    .expect("refresh");
    let owner = SemanticOwner::new(
        context().context_key(),
        fixture.resource("src/Parent.svelte").id,
    );
    assert_eq!(
        index.status(&owner).expect("status").state,
        SemanticState::Current
    );

    let broken = ScriptedBackend::new().failing(RequestFailure::Backend(HostError::new("gone")));
    let error = refresh(&fixture, &index, &broken, "src/Parent.svelte")
        .expect_err("a dead backend is not an empty success");
    assert!(matches!(
        error,
        super::SvelteSemanticError::Backend(adapter::AdapterError::Request(_))
    ));
    assert!(index.status(&owner).expect("status").has_last_valid());
}

#[test]
fn an_unimplemented_method_is_an_error_not_zero_findings() {
    let fixture = Fixture::create("unsupported");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let error = refresh(
        &fixture,
        &index,
        &ScriptedBackend::new().unsupported(),
        "src/Parent.svelte",
    )
    .expect_err("an unsupported method is not an empty result");
    assert!(matches!(
        error,
        super::SvelteSemanticError::Backend(adapter::AdapterError::Protocol(_))
    ));
}

// ---------------------------------------------------------------------
// The wire
// ---------------------------------------------------------------------

#[test]
fn a_change_batch_is_one_notification_and_no_document_overlay() {
    let fixture = Fixture::create("watched");
    let backend = ScriptedBackend::new();
    let changes = vec![
        ResourceChange::new(
            fixture.resource("src/Parent.svelte").id,
            ChangeKind::Changed,
            "src/Parent.svelte",
        ),
        ResourceChange::new(
            fixture.resource("src/lib/Child.svelte").id,
            ChangeKind::Changed,
            "src/lib/Child.svelte",
        ),
    ];
    assert_eq!(
        lifecycle::synchronize(&backend, &fixture.root, &changes).expect("synchronized"),
        2
    );
    assert_eq!(
        backend.notifications().len(),
        1,
        "one logical change, one notification -- never one per owner"
    );
    let sent: Vec<&'static str> = backend.calls().iter().map(|call| call.wire().0).collect();
    for forbidden in FORBIDDEN_SYNC_METHODS {
        assert!(!sent.contains(&forbidden), "{forbidden}");
    }
}

#[test]
fn a_move_is_two_filesystem_events_because_on_the_filesystem_it_is() {
    let fixture = Fixture::create("watched-move");
    let changes = vec![
        ResourceChange::new(
            fixture.resource("src/Parent.svelte").id,
            ChangeKind::Moved,
            "src/After.svelte",
        )
        .from_previous("src/Before.svelte"),
    ];
    let batch = lifecycle::watched_changes(&fixture.root, &changes);
    let kind_of = |name: &str| {
        batch
            .iter()
            .find(|change| change.uri.ends_with(name))
            .unwrap_or_else(|| panic!("{name} in {batch:?}"))
            .kind
    };
    assert_eq!(kind_of("src/Before.svelte"), WatchedChangeKind::Deleted);
    assert_eq!(kind_of("src/After.svelte"), WatchedChangeKind::Changed);
    let mut sorted = batch.clone();
    sorted.sort_by(|left, right| left.uri.cmp(&right.uri));
    assert_eq!(batch, sorted, "deterministically ordered");
}

#[test]
fn announcing_a_cold_backend_names_every_component_and_nothing_else() {
    let fixture = Fixture::create("announce");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let backend = ScriptedBackend::new();
    let count = lifecycle::announce_components(&index, &backend, &fixture.root).expect("announce");

    let components = fixture
        .resources()
        .iter()
        .filter(|resource| resource.language == Some(ResourceLanguage::Svelte))
        .count();
    assert_eq!(count, components);
    assert!(components >= 5, "the fixture has several components");
    let batch = &backend.notifications()[0];
    assert!(
        batch.iter().all(|change| change.uri.ends_with(".svelte")),
        "a `.ts` belongs to the TypeScript backend: {batch:?}"
    );
}

// ---------------------------------------------------------------------
// Configuration and trust
// ---------------------------------------------------------------------

#[test]
fn a_svelte_config_is_an_input_and_never_executed() {
    let fixture = Fixture::create("config-trust");
    fixture.write(
        "svelte.config.js",
        "import { writeFileSync } from 'node:fs';\nwriteFileSync('EXECUTED', 'x');\nexport default {};\n",
    );
    fixture.rescan("workspace-rev-2");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let config = lifecycle::discover_config(index.connection(), "").expect("config");

    assert!(config.svelte_config.is_some());
    assert!(
        config.has_unexecuted_config(),
        "a project config that exists and is deliberately not run is a \
         stated limitation, not a silent one"
    );
    // Its bytes are still an input: a trusted run would mean something
    // different, and the basis has to move when the file does.
    assert!(
        config.basis().names().any(|name| name == "config_trusted"),
        "the trust decision itself is part of the basis"
    );
    assert!(!fixture.root.join("EXECUTED").exists());
}

#[test]
fn changing_the_config_moves_the_basis() {
    let fixture = Fixture::create("config-change");
    fixture.write("svelte.config.js", "export default { a: 1 };\n");
    fixture.rescan("workspace-rev-2");
    let before = {
        let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
        lifecycle::discover_config(index.connection(), "")
            .expect("config")
            .basis()
            .fingerprint()
    };
    fixture.write("svelte.config.js", "export default { a: 2 };\n");
    fixture.rescan("workspace-rev-3");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    assert_ne!(
        lifecycle::discover_config(index.connection(), "")
            .expect("config")
            .basis()
            .fingerprint(),
        before
    );
}

// ---------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------

fn install(fixture: &Fixture) -> super::SvelteInstall {
    super::SvelteInstall {
        root: fixture.root.clone(),
        entry_point: fixture
            .root
            .join("node_modules/svelte-language-server/bin/server.js"),
        server_version: super::TESTED_SERVER_VERSION.to_owned(),
        companions: vec![
            ("svelte2tsx".to_owned(), Some("0.7.61".to_owned())),
            ("svelte".to_owned(), Some("5.57.1".to_owned())),
            ("typescript".to_owned(), Some("6.0.3".to_owned())),
        ],
    }
}

#[test]
fn every_tool_that_decides_what_a_component_means_is_in_the_identity() {
    let fixture = Fixture::create("env");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let identity =
        lifecycle::environment_identity(index.connection(), &fixture.root, "", &install(&fixture))
            .expect("identity");
    assert_eq!(identity.assurance, EnvironmentAssurance::Proven);
    assert_eq!(identity.lockfile.as_deref(), Some("package-lock.json"));

    // A different Svelte compiler is a different language, and a
    // different svelte2tsx is a different intermediate representation.
    for (index_of, replacement) in [(1_usize, "4.2.19"), (0, "0.7.60"), (2, "5.9.2")] {
        let mut other = install(&fixture);
        other.companions[index_of].1 = Some(replacement.to_owned());
        let moved = lifecycle::environment_identity(
            SemanticIndex::open(&fixture.db_path())
                .expect("index.db")
                .connection(),
            &fixture.root,
            "",
            &other,
        )
        .expect("identity");
        assert_ne!(
            moved.fingerprint, identity.fingerprint,
            "{} must be part of the identity",
            other.companions[index_of].0
        );
    }
}

#[test]
fn without_a_lockfile_the_environment_is_unknown_rather_than_assumed() {
    let fixture = Fixture::create("env-unlocked");
    fixture.remove("package-lock.json");
    fixture.rescan("workspace-rev-2");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let identity =
        lifecycle::environment_identity(index.connection(), &fixture.root, "", &install(&fixture))
            .expect("identity");
    assert!(!identity.assurance.is_proven());
}

// ---------------------------------------------------------------------
// Change plans and Level B
// ---------------------------------------------------------------------

fn published(fixture: &Fixture) -> SemanticIndex {
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    refresh(
        fixture,
        &index,
        &backend_for_parent(fixture),
        "src/Parent.svelte",
    )
    .expect("refresh");
    index
}

#[test]
fn only_the_owners_whose_basis_names_a_changed_resource_are_invalidated() {
    let fixture = Fixture::create("plan-basis");
    let index = published(&fixture);
    let config = lifecycle::discover_config(index.connection(), "").expect("config");
    let parent = SemanticOwner::new(
        context().context_key(),
        fixture.resource("src/Parent.svelte").id,
    );

    let unrelated = ResourceChange::new(
        fixture.resource("src/SameName.svelte").id,
        ChangeKind::Changed,
        "src/SameName.svelte",
    );
    let plan = lifecycle::plan_changes(&index, &context(), &[unrelated], &config).expect("plan");
    assert!(!plan.affected.contains(&parent));

    // A `.ts` the component's proof read is a different matter.
    let depended = ResourceChange::new(
        fixture.resource("src/lib/Child.svelte").id,
        ChangeKind::Changed,
        "src/lib/Child.svelte",
    );
    let plan = lifecycle::plan_changes(&index, &context(), &[depended], &config).expect("plan");
    assert!(
        plan.affected.contains(&parent),
        "the component whose proof read `Child.svelte` is stale"
    );
}

#[test]
fn a_module_config_or_environment_change_invalidates_the_whole_project() {
    let fixture = Fixture::create("plan-wide");
    let index = published(&fixture);
    let config = lifecycle::discover_config(index.connection(), "").expect("config");
    let parent = SemanticOwner::new(
        context().context_key(),
        fixture.resource("src/Parent.svelte").id,
    );

    // A new component changes what every specifier can resolve to.
    let added = ResourceChange::new(
        fixture.resource("src/SameName.svelte").id,
        ChangeKind::Added,
        "src/Fresh.svelte",
    );
    let plan = lifecycle::plan_changes(&index, &context(), &[added], &config).expect("plan");
    assert!(plan.inventory_moved);
    assert!(plan.affected.contains(&parent));

    for (rel, expect_config, expect_environment) in [
        ("tsconfig.json", true, false),
        ("package-lock.json", false, true),
        ("package.json", false, true),
    ] {
        let change = ResourceChange::new(fixture.resource(rel).id, ChangeKind::Changed, rel)
            .with_language(None);
        let plan = lifecycle::plan_changes(&index, &context(), &[change], &config).expect("plan");
        assert_eq!(plan.config_moved, expect_config, "{rel}");
        assert_eq!(plan.environment_moved, expect_environment, "{rel}");
        assert!(plan.affected.contains(&parent), "{rel}");
    }
}

#[test]
fn a_missing_backend_leaves_structural_container_truth_usable() {
    let fixture = Fixture::create("level-b");
    // No refresh at all -- this is a Workspace with no Svelte tools.
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let parent = fixture.resource("src/Parent.svelte");

    // The component is still a Resource, still has its script Symbols,
    // and still has its template use sites.
    let symbols: i64 = index
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM symbol WHERE resource_id = \
             (SELECT id FROM resource WHERE uid = ?1)",
            rusqlite::params![parent.id.to_bytes().to_vec()],
            |row| row.get(0),
        )
        .expect("count");
    assert!(symbols > 0, "structural truth does not need a backend");

    // And nothing claims a semantic answer.
    let owner = SemanticOwner::new(context().context_key(), parent.id);
    let status = index.status(&owner).expect("status");
    assert_ne!(status.state, SemanticState::Current);
    assert_eq!(
        lifecycle::availability(
            &status,
            RuntimeState::Stopped,
            lifecycle::BackendReadiness::Unavailable
        ),
        SemanticAvailability::Unavailable
    );
}

#[test]
fn a_current_proof_survives_a_cold_runtime_and_a_stale_one_does_not() {
    let fixture = Fixture::create("cold");
    let index = published(&fixture);
    let owner = SemanticOwner::new(
        context().context_key(),
        fixture.resource("src/Parent.svelte").id,
    );
    let current = index.status(&owner).expect("status");
    assert_eq!(
        lifecycle::availability(
            &current,
            RuntimeState::Stopped,
            lifecycle::BackendReadiness::Unavailable
        ),
        SemanticAvailability::CurrentRuntimeCold
    );

    index
        .mark_dirty(&owner, crate::semantic_index::SOURCE_MOVED_CODE)
        .expect("mark");
    let stale = index.status(&owner).expect("status");
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
fn a_daemon_reopen_cannot_restore_an_unprovable_environment_as_current() {
    let fixture = Fixture::create("reopen");
    let index = published(&fixture);
    let config = lifecycle::discover_config(index.connection(), "")
        .expect("config")
        .basis();
    let environment =
        lifecycle::environment_identity(index.connection(), &fixture.root, "", &install(&fixture))
            .expect("identity");
    let inventory = lifecycle::inventory_fingerprint(index.connection()).expect("inventory");
    let capabilities = super::capability_report(&context());

    let proven =
        lifecycle::current_inputs(&context(), &config, &capabilities, &inventory, &environment);
    assert!(
        lifecycle::revalidate_context(&index, &context(), &proven)
            .expect("revalidate")
            .iter()
            .any(|(_, status)| status.state == SemanticState::Current),
        "a publication whose inputs still hold proves itself from disk"
    );

    let unprovable = lifecycle::EnvironmentIdentity {
        assurance: EnvironmentAssurance::Unknown {
            reason: "no lockfile".to_owned(),
        },
        ..environment
    };
    let guarded =
        lifecycle::current_inputs(&context(), &config, &capabilities, &inventory, &unprovable);
    assert!(
        lifecycle::revalidate_context(&index, &context(), &guarded)
            .expect("revalidate")
            .iter()
            .all(|(_, status)| status.state != SemanticState::Current)
    );
}

// ---------------------------------------------------------------------
// The capability report
// ---------------------------------------------------------------------

#[test]
fn the_report_claims_the_two_capabilities_this_tier_exists_for() {
    let declared = super::capability_report(&context());
    assert_eq!(
        declared.support(SemanticCapability::EmbeddedRegionMapping),
        super::EMBEDDED_REGION_MAPPING
    );
    assert_eq!(
        declared.support(SemanticCapability::OriginalSourceMapping),
        super::ORIGINAL_SOURCE_MAPPING
    );
    // And claims nothing a Svelte component cannot do.
    for absent in [
        SemanticCapability::Inheritance,
        SemanticCapability::Implements,
        SemanticCapability::Overrides,
        SemanticCapability::OverloadResolution,
    ] {
        assert_eq!(
            declared.support(absent),
            crate::resolution::Support::Unsupported,
            "{absent:?}"
        );
    }
}

#[test]
fn a_self_answer_is_not_an_edge() {
    // A definition that lands on the site it was asked about is a
    // binding naming itself. An edge from a site to itself says nothing,
    // so it is not published.
    let fixture = Fixture::create("self-answer");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let increment_use = fixture.offset_of("src/Parent.svelte", "increment}", 0);
    let at = fixture.last_character(
        "src/Parent.svelte",
        increment_use,
        increment_use + "increment".len(),
    );
    let backend = ScriptedBackend::new().with_definition(
        &fixture.uri("src/Parent.svelte"),
        at,
        vec![fixture.component_location("src/Parent.svelte")],
    );
    let outcome = refresh(&fixture, &index, &backend, "src/Parent.svelte").expect("refresh");
    assert!(
        outcome
            .report
            .iter()
            .all(|line| !line.contains("-> RESOLVED Resource")),
        "a component referencing itself is not a fact: {:?}",
        outcome.report
    );
}

#[test]
fn a_delivered_notification_needs_no_reply() {
    let backend = ScriptedBackend::new();
    assert_eq!(
        backend
            .call(&SvelteRequest::WatchedFilesChanged {
                changes: Vec::new()
            })
            .expect("delivered"),
        SvelteResponse::Delivered
    );
}

#[test]
fn evidence_anchors_only_on_occurrences_that_already_exist() {
    let fixture = Fixture::create("anchored");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let owner = fixture.resource("src/Parent.svelte");
    let text = fixture.text("src/Parent.svelte");
    let gaps =
        crate::evidence::list_unresolved_for_resource(index.connection(), owner.id).expect("gaps");
    let sites =
        adapter::collect_sites(index.connection(), &owner, &text, &gaps, "ctx").expect("sites");
    let occurrences =
        crate::symbol::list_occurrences_for_resource(index.connection(), owner.id).expect("occ");
    assert!(!sites.sites.is_empty(), "the component offers sites");
    for site in &sites.sites {
        assert!(
            occurrences.iter().any(|occurrence| {
                occurrence.kind == site.occurrence.kind
                    && occurrence.span.start_byte == site.occurrence.start_byte
                    && occurrence.span.end_byte == site.occurrence.end_byte
            }),
            "{site:?} has no Occurrence"
        );
        assert!(
            site.occurrence.end_byte <= text.len(),
            "every site indexes the current component source"
        );
    }
}

#[test]
fn a_template_local_never_becomes_an_edge_to_a_symbol_it_does_not_name() {
    // `{#each items as item}` introduces `item`, which the graph has no
    // Symbol model for. The rule that keeps it out is binding, not
    // naming -- and it must not take the surrounding script references
    // with it.
    let fixture = Fixture::create("template-local");
    fixture.write(
        "src/Each.svelte",
        "<script lang=\"ts\">\n    let items: string[] = [];\n</script>\n\n\
         {#each items as item}\n    <p>{item}</p>\n{/each}\n",
    );
    fixture.rescan("workspace-rev-2");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let owner = fixture.resource("src/Each.svelte");
    let text = fixture.text("src/Each.svelte");
    let occurrences =
        crate::symbol::list_occurrences_for_resource(index.connection(), owner.id).expect("occ");
    let uses: Vec<&str> = occurrences
        .iter()
        .filter(|occurrence| occurrence.kind == crate::symbol::OccurrenceKind::ReferenceSite)
        .map(|occurrence| &text[occurrence.span.start_byte..occurrence.span.end_byte])
        .collect();
    assert_eq!(uses, vec!["items"], "{uses:?}");
}

#[test]
fn a_relation_kind_is_never_invented_for_a_framework_fact() {
    // Task 11 adds no RelationKind. A component usage is a reference,
    // because that is what it is.
    let fixture = Fixture::create("no-new-kinds");
    let index = published(&fixture);
    let mut statement = index
        .connection()
        .prepare("SELECT DISTINCT kind FROM relation")
        .expect("prepare");
    let kinds: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("kinds");
    for kind in &kinds {
        assert!(
            RelationKind::parse(kind).is_ok(),
            "{kind} is not a canonical relation kind"
        );
    }
    assert!(kinds.contains(&RelationKind::References.as_str().to_owned()));
}

#[test]
fn an_outcome_names_a_workspace_endpoint_or_nothing() {
    let fixture = Fixture::create("endpoints");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let outcome = refresh(
        &fixture,
        &index,
        &backend_for_parent(&fixture),
        "src/Parent.svelte",
    )
    .expect("refresh");
    assert!(outcome.evidence_count > 0);
    // Nothing generated, and nothing outside the Workspace, reached the
    // report as a resolved endpoint.
    for line in &outcome.report {
        assert!(!line.contains("__sveltets"), "{line}");
    }
    let _ = GraphEndpoint::Resource(fixture.resource("src/Parent.svelte").id);
    let _ = SemanticOutcome::Unresolved {
        reason: crate::gaps::UnresolvedReason::NoStructuralBinding,
    };
}
