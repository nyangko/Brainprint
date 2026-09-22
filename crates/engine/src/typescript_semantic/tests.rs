//! Always-on tests for the TypeScript/JavaScript adapter.
//!
//! No `tsc`, no Node, no network. What is checked here is the half of
//! the tier that does not depend on a backend being installed: how a
//! location becomes identity, which sites are asked about at all, and
//! what task 4 does with the answers. The real server's answers are
//! checked by `tests/i4_typescript_acceptance.rs`, which is ignored by
//! default.

use std::{collections::BTreeSet, path::Path};

use super::{
    adapter::{
        self, EXTERNAL_PACKAGE, EXTERNAL_SYMBOL, Normalizer, ResolvableKind, TypeScriptQueries,
        external_entity, override_keyword_members,
    },
    lifecycle,
    protocol::{TypeScriptRequest, TypeScriptResponse},
    tests_support::{
        Fixture, ScriptedBackend, backend_for_service_run, context, encoding, refresh,
    },
};
use crate::{
    graph::{GraphEndpoint, RelationKind},
    lsp::coordinates::{Position, Range},
    resolution::Support,
    runtime::{HostError, RequestFailure},
    semantic::{SemanticCapability, SemanticOutcome},
    semantic_index::{SemanticIndex, SemanticOwner, SemanticState},
    symbol::OccurrenceKind,
};

// ---------------------------------------------------------------------
// External identity
// ---------------------------------------------------------------------

#[test]
fn a_dependency_declaration_gets_a_package_identity_without_being_read() {
    let entity = external_entity(
        Path::new("/w/node_modules/@types/node/path.d.ts"),
        Some("join"),
    )
    .expect("named");
    // `@scope/name` is one package name, and `path.d.ts` is the module
    // `path` -- the declaration file and the implementation are the
    // same module, so they collapse to one identity.
    assert_eq!(entity.package_identity, "@types/node");
    assert_eq!(entity.module_path.as_deref(), Some("path"));
    assert_eq!(entity.symbol_name.as_deref(), Some("join"));
    assert_eq!(entity.qualified_name.as_deref(), Some("path.join"));
    assert_eq!(entity.kind, EXTERNAL_SYMBOL);
    // A machine path must never reach canonical identity.
    assert_eq!(entity.declaration_locator, None);
}

#[test]
fn a_packages_entry_point_is_the_package_itself() {
    let entity =
        external_entity(Path::new("/w/node_modules/left-pad/index.d.ts"), None).expect("named");
    assert_eq!(entity.package_identity, "left-pad");
    assert_eq!(entity.module_path, None);
    assert_eq!(entity.kind, EXTERNAL_PACKAGE);
}

#[test]
fn a_nested_dependency_belongs_to_the_nearest_package() {
    // pnpm and npm both produce nested trees; the package a file
    // belongs to is the one after the *last* `node_modules`.
    let entity = external_entity(
        Path::new("/w/node_modules/a/node_modules/b/lib/deep.d.ts"),
        Some("thing"),
    )
    .expect("named");
    assert_eq!(entity.package_identity, "b");
    assert_eq!(entity.module_path.as_deref(), Some("lib/deep"));
}

#[test]
fn a_path_outside_any_package_gets_no_invented_identity() {
    assert!(external_entity(Path::new("/elsewhere/lib/thing.d.ts"), Some("thing")).is_none());
    assert!(external_entity(Path::new("/w/node_modules"), None).is_none());
}

// ---------------------------------------------------------------------
// Site selection
// ---------------------------------------------------------------------

#[test]
fn a_site_the_parser_bound_to_a_package_is_left_to_the_parser() {
    let fixture = Fixture::create("sites-external");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let owner = fixture.resource("src/consumer.ts");
    let text = fixture.text("src/consumer.ts");
    let gaps =
        crate::evidence::list_unresolved_for_resource(index.connection(), owner.id).expect("gaps");
    let sites =
        adapter::collect_sites(index.connection(), &owner, &text, &gaps, "ctx").expect("sites");

    let specifier = text.find("\"@core/service\"").expect("specifier");
    assert!(
        sites
            .retained_external
            .iter()
            .any(|retained| retained.occurrence.start_byte == specifier),
        "the `@core/*` specifier is classified by shape, not by reading \
         `paths`; re-proving it would report the project's own alias as a \
         contradiction forever"
    );
    assert!(
        !sites
            .sites
            .iter()
            .any(|site| site.occurrence.start_byte == specifier),
        "and it is not asked about"
    );
    // The *names* that specifier binds are asked about, which is where
    // the alias becomes a Workspace Symbol.
    let service = text.find("Service, unwrap").expect("named import");
    assert!(
        sites
            .sites
            .iter()
            .any(|site| site.occurrence.start_byte == service
                && site.kind == ResolvableKind::Imports),
        "{:?}",
        sites.sites
    );
}

#[test]
fn a_type_site_whose_relation_nothing_states_is_not_guessed() {
    // `extends`, `implements` and an annotation are three relations and
    // a TYPE_SITE does not say which. Where I3 states one -- as a gap
    // or as a bound relation -- it is used; where it states none, the
    // site is skipped rather than defaulted.
    let fixture = Fixture::create("sites-typed");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let owner = fixture.resource("src/hierarchy.ts");
    let text = fixture.text("src/hierarchy.ts");
    let gaps =
        crate::evidence::list_unresolved_for_resource(index.connection(), owner.id).expect("gaps");
    let sites =
        adapter::collect_sites(index.connection(), &owner, &text, &gaps, "ctx").expect("sites");

    let implements = text.find("Runner {").expect("implements clause");
    let extends = text.find("Base implements").expect("extends clause");
    assert_eq!(
        sites
            .sites
            .iter()
            .find(|site| site.occurrence.start_byte == implements)
            .map(|site| site.kind),
        Some(ResolvableKind::Implements),
        "the gap states IMPLEMENTS"
    );
    assert_eq!(
        sites
            .sites
            .iter()
            .find(|site| site.occurrence.start_byte == extends)
            .map(|site| site.kind),
        Some(ResolvableKind::Extends),
        "and the already-bound relation states EXTENDS"
    );
}

#[test]
fn an_override_claim_is_read_from_the_declarations_own_modifiers() {
    let fixture = Fixture::create("override-keyword");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let owner = fixture.resource("src/hierarchy.ts");
    let text = fixture.text("src/hierarchy.ts");
    let symbols = crate::symbol::list_for_resource(index.connection(), owner.id).expect("symbols");
    let occurrences =
        crate::symbol::list_occurrences_for_resource(index.connection(), owner.id).expect("occ");
    let declared = override_keyword_members(&text, &symbols, &occurrences);

    let child_run = symbols
        .iter()
        .find(|symbol| text[symbol.span.start_byte..].starts_with("override run"))
        .expect("Child.run");
    assert!(declared.contains(&child_run.id));
    // Every other `run` in the file writes no modifier, and none of
    // them is picked up by name.
    assert_eq!(declared.len(), 1, "{declared:?}");
}

// ---------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------

/// Resolve one site through a scripted backend and return the outcome.
fn resolve_one(
    fixture: &Fixture,
    rel: &str,
    needle: &str,
    answer: Vec<crate::typescript_semantic::protocol::Location>,
) -> SemanticOutcome {
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let owner = fixture.resource(rel);
    let text = fixture.text(rel);
    let start = text.find(needle).expect("needle");
    let end = start + needle.len();
    let at = fixture.last_character(rel, start, end);
    let backend = ScriptedBackend::new().with_definition(&fixture.uri(rel), at, answer);

    let gaps =
        crate::evidence::list_unresolved_for_resource(index.connection(), owner.id).expect("gaps");
    let mut sites =
        adapter::collect_sites(index.connection(), &owner, &text, &gaps, "ctx").expect("sites");
    sites
        .sites
        .retain(|site| site.occurrence.start_byte == start && site.occurrence.end_byte == end);
    assert_eq!(sites.sites.len(), 1, "{needle:?} is one site");

    let occurrences =
        crate::symbol::list_occurrences_for_resource(index.connection(), owner.id).expect("occ");
    let mut normalizer = Normalizer::new(index.connection(), &fixture.root, encoding());
    let produced = adapter::resolve_resource(
        &backend,
        &mut normalizer,
        &adapter::ResourceRequest {
            owner: &owner,
            owner_text: &text,
            sites: &sites,
            occurrences: &occurrences,
            generation_id: 0,
            analysis_profile_id: 1,
            context_key: "ctx",
        },
    )
    .expect("resolved");
    produced.evidence[0].outcome.clone()
}

#[test]
fn a_definition_answer_becomes_the_symbol_at_that_exact_span() {
    let fixture = Fixture::create("normalize-symbol");
    let text = fixture.text("src/hierarchy.ts");
    // `Beta.run`'s own declaration name token.
    let beta = text.find("class Beta { run").expect("Beta") + "class Beta { ".len();
    let answer = fixture.location("src/hierarchy.ts", beta, beta + 3);
    let outcome = resolve_one(&fixture, "src/hierarchy.ts", "b.run", vec![answer]);
    let SemanticOutcome::Resolved {
        target: GraphEndpoint::Symbol(_),
    } = outcome
    else {
        panic!("expected one Symbol: {outcome:?}");
    };
}

#[test]
fn a_range_that_does_not_convert_exactly_is_refused_rather_than_clamped() {
    let fixture = Fixture::create("normalize-clamp");
    // A range past the end of the file. The nearest span is a different
    // symbol, so the honest answer is no answer.
    let answer = crate::typescript_semantic::protocol::Location {
        uri: fixture.uri("src/hierarchy.ts"),
        range: Range::new(Position::new(9_000, 0), Position::new(9_000, 3)),
    };
    let outcome = resolve_one(&fixture, "src/hierarchy.ts", "b.run", vec![answer]);
    assert!(
        matches!(outcome, SemanticOutcome::Unresolved { .. }),
        "{outcome:?}"
    );
}

#[test]
fn two_distinct_workspace_targets_stay_candidates_rather_than_a_pick() {
    let fixture = Fixture::create("normalize-candidates");
    let text = fixture.text("src/samename.ts");
    let first = text.find("class A { run").expect("A") + "class A { ".len();
    let second = text.find("class B { run").expect("B") + "class B { ".len();
    let outcome = resolve_one(
        &fixture,
        "src/hierarchy.ts",
        "b.run",
        vec![
            fixture.location("src/samename.ts", first, first + 3),
            fixture.location("src/samename.ts", second, second + 3),
        ],
    );
    let SemanticOutcome::Candidates { targets } = outcome else {
        panic!("expected candidates: {outcome:?}");
    };
    assert_eq!(targets.len(), 2, "ordering is not evidence");
}

#[test]
fn declaration_merging_across_one_packages_files_is_one_target() {
    let fixture = Fixture::create("normalize-merged");
    // What the real server answers for `String`: nine `lib.*.d.ts`
    // files of one package, all contributing to one merged interface.
    let base = fixture.root.join("node_modules").join("typescript");
    let answers = ["lib.es5.d.ts", "lib.es2015.core.d.ts", "lib.dom.d.ts"]
        .into_iter()
        .map(|file| crate::typescript_semantic::protocol::Location {
            uri: crate::typescript_semantic::protocol::path_to_uri(&base.join("lib").join(file)),
            range: Range::new(Position::new(1, 10), Position::new(1, 16)),
        })
        .collect();
    let outcome = resolve_one(&fixture, "src/consumer.ts", "s.run", answers);
    let SemanticOutcome::Resolved {
        target: GraphEndpoint::External(entity),
    } = outcome
    else {
        panic!("one merged declaration: {outcome:?}");
    };
    assert_eq!(entity.package_identity, "typescript");
    assert_eq!(
        entity.module_path, None,
        "which declaration file contributed is not identity"
    );
}

#[test]
fn a_method_the_backend_does_not_implement_is_an_error_not_zero_findings() {
    let fixture = Fixture::create("unsupported");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let owner = fixture.resource("src/consumer.ts");
    let text = fixture.text("src/consumer.ts");
    let gaps =
        crate::evidence::list_unresolved_for_resource(index.connection(), owner.id).expect("gaps");
    let sites =
        adapter::collect_sites(index.connection(), &owner, &text, &gaps, "ctx").expect("sites");
    let occurrences =
        crate::symbol::list_occurrences_for_resource(index.connection(), owner.id).expect("occ");
    let mut normalizer = Normalizer::new(index.connection(), &fixture.root, encoding());
    let error = adapter::resolve_resource(
        &ScriptedBackend::new().unsupported(),
        &mut normalizer,
        &adapter::ResourceRequest {
            owner: &owner,
            owner_text: &text,
            sites: &sites,
            occurrences: &occurrences,
            generation_id: 0,
            analysis_profile_id: 1,
            context_key: "ctx",
        },
    )
    .expect_err("an unsupported method is not an empty result");
    assert!(
        matches!(error, adapter::AdapterError::Protocol(_)),
        "{error}"
    );
}

// ---------------------------------------------------------------------
// Through publication and merge
// ---------------------------------------------------------------------

#[test]
fn a_resolved_gap_becomes_a_canonical_edge_and_a_second_refresh_changes_nothing() {
    let fixture = Fixture::create("merge-idempotent");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let backend = backend_for_service_run(&fixture);

    let first = refresh(&fixture, &index, &backend, "src/consumer.ts").expect("first");
    assert_eq!(first.merged.gaps_resolved, 1);
    assert_eq!(first.merged.relations_created, 1);
    assert_eq!(first.merged.conflicts, 0);

    let relations: i64 = index
        .connection()
        .query_row("SELECT COUNT(*) FROM relation", [], |row| row.get(0))
        .expect("count");
    let second = refresh(&fixture, &index, &backend, "src/consumer.ts").expect("second");
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
fn a_backend_that_disagrees_with_the_parser_records_a_conflict_and_no_edge() {
    let fixture = Fixture::create("merge-conflict");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    // `unwrap(b)` is a call site the parser already bound. Answer it
    // with a different, internal target and neither tier may win.
    let text = fixture.text("src/consumer.ts");
    let call = text.rfind("unwrap(b)").expect("call site");
    let at = fixture.last_character("src/consumer.ts", call, call + "unwrap".len());
    let model = fixture.text("src/model.ts");
    let describe = model.find("describe").expect("Model.describe");
    let backend = ScriptedBackend::new().with_definition(
        &fixture.uri("src/consumer.ts"),
        at,
        vec![fixture.location("src/model.ts", describe, describe + "describe".len())],
    );

    // The parser bound `unwrap` to an external symbol, so the adapter
    // leaves it alone -- there is nothing to conflict with.
    let outcome = refresh(&fixture, &index, &backend, "src/consumer.ts").expect("refresh");
    assert!(
        outcome
            .retained_external
            .iter()
            .any(|retained| retained.kind == RelationKind::Calls),
        "an external call binding is retained, not contradicted"
    );
    assert_eq!(outcome.merged.conflicts, 0);

    // A site bound to an *internal* target is a different matter: two
    // tiers proving one site differently is a real disagreement, and
    // task 4 records it rather than picking a winner.
    let use_esm = fixture.text("js/use-esm.js");
    let esm_call = use_esm.find("esmFn(").expect("call");
    let at = fixture.last_character("js/use-esm.js", esm_call, esm_call + "esmFn".len());
    let backend = ScriptedBackend::new().with_definition(
        &fixture.uri("js/use-esm.js"),
        at,
        vec![fixture.location("src/model.ts", describe, describe + "describe".len())],
    );
    let outcome = refresh(&fixture, &index, &backend, "js/use-esm.js").expect("refresh");
    assert_eq!(outcome.merged.conflicts, 1, "{:?}", outcome.report);
    assert_eq!(
        outcome.merged.relations_created, 0,
        "a disagreement creates no edge"
    );
}

#[test]
fn withdrawing_a_contribution_restores_the_gap_it_displaced() {
    let fixture = Fixture::create("merge-withdraw");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let backend = backend_for_service_run(&fixture);
    refresh(&fixture, &index, &backend, "src/consumer.ts").expect("refresh");

    let owner = SemanticOwner::new(
        context().context_key(),
        fixture.resource("src/consumer.ts").id,
    );
    let before: i64 = index
        .connection()
        .query_row("SELECT COUNT(*) FROM unresolved_reference", [], |row| {
            row.get(0)
        })
        .expect("count");
    let report = lifecycle::withdraw_affected(
        &index,
        &BTreeSet::from([owner]),
        crate::semantic_index::SOURCE_MOVED_CODE,
    )
    .expect("withdraw");
    assert_eq!(report.gaps_restored, 1);
    assert_eq!(report.relations_removed, 1);
    assert_eq!(
        index
            .connection()
            .query_row("SELECT COUNT(*) FROM unresolved_reference", [], |row| row
                .get::<_, i64>(
                0
            ))
            .expect("count"),
        before + 1,
        "an honest gap comes back, not a silence"
    );
}

#[test]
fn a_backend_failure_leaves_the_graph_and_the_last_publication_alone() {
    let fixture = Fixture::create("merge-failure");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let backend = backend_for_service_run(&fixture);
    refresh(&fixture, &index, &backend, "src/consumer.ts").expect("refresh");
    let owner = SemanticOwner::new(
        context().context_key(),
        fixture.resource("src/consumer.ts").id,
    );
    assert_eq!(
        index.status(&owner).expect("status").state,
        SemanticState::Current
    );

    let broken = ScriptedBackend::new().failing(RequestFailure::Backend(HostError::new("gone")));
    let error = refresh(&fixture, &index, &broken, "src/consumer.ts")
        .expect_err("a dead backend is not an empty success");
    assert!(matches!(
        error,
        super::TypeScriptSemanticError::Backend(adapter::AdapterError::Request(_))
    ));
    assert!(
        index.status(&owner).expect("status").has_last_valid(),
        "the last valid publication is kept"
    );
}

#[test]
fn evidence_carries_the_capability_the_site_answered_under() {
    let fixture = Fixture::create("capabilities");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let backend = backend_for_service_run(&fixture);
    let outcome = refresh(&fixture, &index, &backend, "src/consumer.ts").expect("refresh");
    assert!(
        outcome
            .report
            .iter()
            .any(|line| line.starts_with("CallsCrossFile")),
        "a call through a receiver is recorded as one: {:?}",
        outcome.report
    );
    // And the declared report agrees with what the tier does.
    let declared = super::capability_report(&context());
    assert_eq!(
        declared.support(SemanticCapability::Implements),
        Support::Supported
    );
    assert_eq!(
        declared.support(SemanticCapability::ResourceDiscovery),
        Support::Unsupported,
        "structure is I2/I3's, and this report does not take credit for it"
    );
}

// ---------------------------------------------------------------------
// The wire, at the adapter's edge
// ---------------------------------------------------------------------

#[test]
fn a_watched_file_batch_is_one_notification_however_many_owners_it_touches() {
    let fixture = Fixture::create("watched");
    let backend = ScriptedBackend::new();
    let changes = vec![
        lifecycle::ResourceChange::new(
            fixture.resource("src/model.ts").id,
            lifecycle::ChangeKind::Changed,
            "src/model.ts",
        ),
        lifecycle::ResourceChange::new(
            fixture.resource("src/public.ts").id,
            lifecycle::ChangeKind::Changed,
            "src/public.ts",
        ),
    ];
    let delivered =
        lifecycle::synchronize(&backend, &fixture.root, &changes).expect("synchronized");
    assert_eq!(delivered, 2);
    assert_eq!(
        backend.notifications().len(),
        1,
        "one logical change, one notification -- never one per owner"
    );
    assert!(
        backend
            .calls()
            .iter()
            .all(|call| matches!(call, TypeScriptRequest::WatchedFilesChanged { .. })),
        "and nothing else went on the wire"
    );
}

#[test]
fn the_adapter_never_emits_a_document_synchronization_method() {
    let fixture = Fixture::create("no-overlay");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let backend = backend_for_service_run(&fixture);
    refresh(&fixture, &index, &backend, "src/consumer.ts").expect("refresh");
    let sent: BTreeSet<&'static str> = backend.calls().iter().map(|call| call.wire().0).collect();
    for forbidden in super::protocol::FORBIDDEN_SYNC_METHODS {
        assert!(!sent.contains(forbidden), "{forbidden}");
    }
}

#[test]
fn a_notification_is_delivered_without_waiting_for_a_reply() {
    let backend = ScriptedBackend::new();
    let answer = backend
        .call(&TypeScriptRequest::WatchedFilesChanged {
            changes: Vec::new(),
        })
        .expect("delivered");
    assert_eq!(answer, TypeScriptResponse::Delivered);
}

#[test]
fn an_import_site_and_a_call_site_are_asked_about_at_different_places() {
    // A module specifier is asked about *inside* the quotes; a name at
    // its last character, because a call site spans the whole callee
    // and asking at the start of `s.run` answers about `s`.
    let fixture = Fixture::create("ask-positions");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let owner = fixture.resource("src/consumer.ts");
    let text = fixture.text("src/consumer.ts");
    let gaps =
        crate::evidence::list_unresolved_for_resource(index.connection(), owner.id).expect("gaps");
    let sites =
        adapter::collect_sites(index.connection(), &owner, &text, &gaps, "ctx").expect("sites");
    let occurrences =
        crate::symbol::list_occurrences_for_resource(index.connection(), owner.id).expect("occ");
    let backend = ScriptedBackend::new();
    let mut normalizer = Normalizer::new(index.connection(), &fixture.root, encoding());
    adapter::resolve_resource(
        &backend,
        &mut normalizer,
        &adapter::ResourceRequest {
            owner: &owner,
            owner_text: &text,
            sites: &sites,
            occurrences: &occurrences,
            generation_id: 0,
            analysis_profile_id: 1,
            context_key: "ctx",
        },
    )
    .expect("resolved");

    let specifier = text.find("\"./public.js\"").expect("specifier");
    let asked: BTreeSet<Position> = backend
        .calls()
        .iter()
        .filter_map(|call| match call {
            TypeScriptRequest::Definition { position, .. } => Some(*position),
            _ => None,
        })
        .collect();
    assert!(
        asked.contains(&fixture.position_of("src/consumer.ts", specifier + 1)),
        "the specifier is asked about inside its quotes"
    );
    let call = text.find("s.run").expect("call site");
    assert!(
        asked.contains(&fixture.last_character("src/consumer.ts", call, call + "s.run".len())),
        "and a call at its last character, not its first"
    );
    assert!(
        !asked.contains(&fixture.position_of("src/consumer.ts", call)),
        "asking at `s` would answer about the receiver -- a real, wrong target"
    );
}

#[test]
fn every_site_the_adapter_answers_anchors_on_an_occurrence_that_already_exists() {
    let fixture = Fixture::create("anchored");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let owner = fixture.resource("src/consumer.ts");
    let text = fixture.text("src/consumer.ts");
    let gaps =
        crate::evidence::list_unresolved_for_resource(index.connection(), owner.id).expect("gaps");
    let sites =
        adapter::collect_sites(index.connection(), &owner, &text, &gaps, "ctx").expect("sites");
    let occurrences =
        crate::symbol::list_occurrences_for_resource(index.connection(), owner.id).expect("occ");
    for site in &sites.sites {
        assert!(
            occurrences.iter().any(|occurrence| {
                occurrence.kind == site.occurrence.kind
                    && occurrence.span.start_byte == site.occurrence.start_byte
                    && occurrence.span.end_byte == site.occurrence.end_byte
            }),
            "{site:?} has no Occurrence"
        );
        assert_ne!(
            site.occurrence.kind,
            OccurrenceKind::Definition,
            "a declaration is not a use site"
        );
    }
}
