//! Always-on tests for the Python semantic backend.
//!
//! None of these needs Node or Pyright. The backend is scripted with
//! exactly the answers #19 task 5 measured the real one giving, over a
//! real indexed Workspace, so the normalization, snapshot, publication
//! and merge behaviour is exercised end to end without the workspace
//! suite depending on anyone's install.

use std::{
    io::Write,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use serde_json::{Value, json};

use super::{
    adapter::{
        self, Batch, BatchError, BatchPolicy, Normalizer, PythonQueries, ResourceRequest,
        resolve_resource, run_batch,
    },
    coordinates::{Position, Range},
    host::{PyrightHost, PythonSettings},
    jsonrpc::{Client, IgnoreServer, testing},
    launcher::{self, PyrightInstall, PythonLauncher},
    protocol::{
        self, Location, ProtocolCompatibility, PythonRequest, TypeAnswer, WatchedChange,
        WatchedChangeKind,
    },
    tests_support::{Fixture, ScriptedBackend, context},
    *,
};
use crate::{
    evidence::{OccurrenceRef, list_unresolved_for_resource},
    gaps::{IntendedRelation, PersistedUnresolved, UnresolvedReason},
    graph::{GraphEndpoint, GraphStore, RelationKind},
    relations::RelationIndex,
    resolution::{Dispatch, Support},
    runtime::{
        CancelToken, HostConcurrency, HostError, HostHealth, RequestFailure, RuntimePolicy,
        SemanticBackendLauncher, SemanticRuntimeHost, SemanticRuntimeSupervisor, StartFailure,
    },
    semantic::{AnalysisContextBinding, SemanticBackendKind, SemanticCapability, SemanticOutcome},
    semantic_index::{SemanticIndex, SemanticIndexError, SemanticState},
    symbol::{OccurrenceKind, list_occurrences_for_resource},
};

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

fn binding() -> AnalysisContextBinding {
    AnalysisContextBinding {
        context: context(),
        project_root_rel: String::new(),
        config_file_rel: None,
    }
}

/// The scripted answers task 5 measured for `pkg/impl.py`.
fn impl_backend(fixture: &Fixture) -> ScriptedBackend {
    let impl_uri = fixture.uri("pkg/impl.py");
    ScriptedBackend::new()
        .with_search_paths(vec![
            protocol::path_to_uri(std::path::Path::new("/typeshed/stdlib")),
            protocol::path_to_uri(std::path::Path::new("/venv/site-packages")),
        ])
        // `x.run(1)` -> Base.run, asked at the last character of the
        // callee span, which is inside `run` and not on the receiver.
        .with_definition(
            &impl_uri,
            last_character(fixture, "pkg/impl.py", "x.run", 0),
            vec![fixture.location("pkg/base.py", "run", 0)],
        )
        // `json.dumps(...)` -> the stub and the implementation of the
        // same dependency module.
        .with_definition(
            &impl_uri,
            last_character(fixture, "pkg/impl.py", "json.dumps", 0),
            vec![
                Location {
                    uri: protocol::path_to_uri(std::path::Path::new(
                        "/typeshed/stdlib/json/__init__.pyi",
                    )),
                    range: Range::new(Position::new(10, 4), Position::new(10, 9)),
                },
                Location {
                    uri: protocol::path_to_uri(std::path::Path::new(
                        "/venv/site-packages/json/__init__.py",
                    )),
                    range: Range::new(Position::new(3, 4), Position::new(3, 9)),
                },
            ],
        )
        // The shadowed `value` reference resolves to the parameter's
        // own declaration, which has no Symbol of its own.
        .with_definition(
            &impl_uri,
            last_character(fixture, "pkg/impl.py", "value", 1),
            Vec::new(),
        )
        // `int` and `str` are builtins in the stdlib stub.
        .with_type(
            &impl_uri,
            fixture.range("pkg/impl.py", "int", 0),
            Some(TypeAnswer {
                declaration: Some(Location {
                    uri: protocol::path_to_uri(std::path::Path::new(
                        "/typeshed/stdlib/builtins.pyi",
                    )),
                    range: Range::new(Position::new(251, 0), Position::new(362, 57)),
                }),
                module_name: None,
                module_uri: None,
            }),
        )
        .with_type(
            &impl_uri,
            fixture.range("pkg/impl.py", "str", 0),
            Some(TypeAnswer {
                declaration: Some(Location {
                    uri: protocol::path_to_uri(std::path::Path::new(
                        "/typeshed/stdlib/builtins.pyi",
                    )),
                    range: Range::new(Position::new(485, 0), Position::new(725, 57)),
                }),
                module_name: None,
                module_uri: None,
            }),
        )
}

/// The position of the last character of the nth `needle`, which is
/// where the adapter asks.
fn last_character(fixture: &Fixture, rel: &str, needle: &str, nth: usize) -> Position {
    let range = fixture.range(rel, needle, nth);
    Position::new(range.end.line, range.end.character - 1)
}

/// Resolve one Resource's real I3 gaps against a scripted backend.
fn resolve(
    fixture: &Fixture,
    backend: &dyn PythonQueries,
    rel: &str,
    extra: &[PersistedUnresolved],
) -> super::ResourceEvidence {
    let store = GraphStore::open(&fixture.db_path()).expect("index.db");
    let connection = store.connection();
    let owner = fixture.resource(rel);
    let text = fixture.text(rel);
    let occurrences = list_occurrences_for_resource(connection, owner.id).expect("occurrences");
    let mut gaps = list_unresolved_for_resource(connection, owner.id).expect("gaps");
    gaps.extend_from_slice(extra);
    let context = context();
    let key = context.context_key();
    let owner_uri = fixture.uri(rel);

    let mut collected = None;
    run_batch(backend, BatchPolicy::default(), &mut |batch: &Batch<'_>| {
        let paths = adapter::search_paths(batch, &owner_uri)?;
        let mut normalizer = Normalizer::new(connection, &fixture.root, paths);
        collected = Some(resolve_resource(
            batch,
            &mut normalizer,
            &ResourceRequest {
                owner: &owner,
                owner_text: &text,
                gaps: &gaps,
                occurrences: &occurrences,
                generation_id: 1,
                analysis_profile_id: 1,
                context_key: &key,
            },
        )?);
        Ok(())
    })
    .expect("batch");
    collected.expect("resolved")
}

fn count(fixture: &Fixture, table: &str) -> i64 {
    GraphStore::open(&fixture.db_path())
        .expect("index.db")
        .connection()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count")
}

fn refresh(
    fixture: &Fixture,
    backend: &dyn PythonQueries,
    rel: &str,
) -> Result<RefreshOutcome, PythonSemanticError> {
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let context = context();
    let capabilities = capability_report(&context);
    let config = config_basis(&PythonSettings::default(), None);
    refresh_resource(
        &index,
        backend,
        &RefreshRequest {
            context: &context,
            workspace_root: &fixture.root,
            owner: fixture.resource(rel).id,
            config: &config,
            capabilities: &capabilities,
            policy: BatchPolicy::default(),
        },
    )
}

// ---------------------------------------------------------------------
// Runtime and launcher
// ---------------------------------------------------------------------

/// A launcher that counts starts without running anything.
struct CountingLauncher {
    starts: AtomicUsize,
    fail: bool,
}

impl SemanticBackendLauncher for CountingLauncher {
    fn kind(&self) -> SemanticBackendKind {
        SemanticBackendKind::Python
    }
    fn launch(
        &self,
        _binding: &AnalysisContextBinding,
    ) -> Result<Arc<dyn SemanticRuntimeHost>, HostError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(HostError::new("no pyright-typeserver install"));
        }
        Err(HostError::new("unused"))
    }
}

#[test]
fn one_analysis_context_starts_one_backend_however_many_callers_ask() {
    let launcher = Arc::new(CountingLauncher {
        starts: AtomicUsize::new(0),
        fail: true,
    });
    let supervisor =
        SemanticRuntimeSupervisor::new(RuntimePolicy::default()).with_backend(launcher.clone());
    let binding = binding();

    // Two callers, one context. The second start is refused by the
    // backoff the first failure opened, not by a second process.
    let first = supervisor.acquire(&binding);
    assert!(matches!(first, Err(StartFailure::Backend(_))));
    let second = supervisor.acquire(&binding);
    assert!(matches!(second, Err(StartFailure::InBackoff { .. })));
    assert_eq!(launcher.starts.load(Ordering::SeqCst), 1);
    assert_eq!(supervisor.live_runtime_count(), 0);
}

#[test]
fn a_missing_backend_leaves_every_structural_answer_intact() {
    let fixture = Fixture::create("degrade");
    let launcher = Arc::new(CountingLauncher {
        starts: AtomicUsize::new(0),
        fail: true,
    });
    let supervisor =
        SemanticRuntimeSupervisor::new(RuntimePolicy::default()).with_backend(launcher);

    let failure = supervisor.acquire(&binding()).expect_err("no install");
    assert!(matches!(failure, StartFailure::Backend(_)));

    // I2/I3 keep working: Resources, Symbols, Occurrences and the
    // structural relation graph are all still there and answerable.
    assert!(fixture.resources().len() >= 5);
    let base = fixture.resource("pkg/base.py");
    let index = RelationIndex::open(&fixture.db_path()).expect("index.db");
    let answer = index
        .outgoing(
            &GraphEndpoint::Resource(fixture.resource("pkg/impl.py").id),
            &[],
        )
        .expect("structural relations still answer");
    assert!(
        answer
            .confirmed
            .iter()
            .any(|relation| relation.target == GraphEndpoint::Resource(base.id)),
        "the structural import edge survives a missing semantic backend"
    );
}

/// A host over an in-process transport, with no child.
fn detached_host() -> (PyrightHost, std::sync::mpsc::Sender<Vec<u8>>) {
    let (to_client, pipe) = testing::Pipe::new();
    let (sink, _from_client) = testing::Sink::new();
    let client = Client::new(Box::new(pipe), Box::new(sink), Arc::new(IgnoreServer));
    (
        PyrightHost::new(
            client,
            None,
            TESTED_PROTOCOL_VERSION.to_owned(),
            ProtocolCompatibility::Tested,
            Arc::new(AtomicU64::new(0)),
        ),
        to_client,
    )
}

#[test]
fn the_host_is_serial_and_reports_a_dead_connection_as_crashed() {
    let (host, to_client) = detached_host();
    assert_eq!(
        host.concurrency(),
        HostConcurrency::Serial,
        "one connection, and task 5 found multi-connection unimplemented"
    );
    assert_eq!(host.health(), HostHealth::Healthy);
    assert_eq!(host.protocol_version(), TESTED_PROTOCOL_VERSION);
    assert_eq!(
        host.resource_usage(),
        crate::runtime::ResourceUsage::UNAVAILABLE
    );

    // End of stream is how a child's death reaches the client.
    drop(to_client);
    for _ in 0..200 {
        if host.health() == HostHealth::Crashed {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(host.health(), HostHealth::Crashed);
}

/// Answer an LSP handshake the way `pyright-typeserver` does.
fn fake_server(
    to_client: std::sync::mpsc::Sender<Vec<u8>>,
    from_client: std::sync::mpsc::Receiver<Vec<u8>>,
    protocol_version: &'static str,
    seen: Arc<Mutex<Vec<String>>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while let Ok(raw) = from_client.recv() {
            let message: Value = serde_json::from_slice(&raw).expect("json");
            let method = message["method"].as_str().unwrap_or_default().to_owned();
            seen.lock().expect("lock").push(method.clone());
            let Some(id) = message.get("id") else {
                continue;
            };
            let result = match method.as_str() {
                "initialize" => json!({"capabilities": {"definitionProvider": true}}),
                "typeServer/getSupportedProtocolVersion" => json!(protocol_version),
                _ => Value::Null,
            };
            if to_client
                .send(testing::frame(
                    &json!({"jsonrpc": "2.0", "id": id, "result": result}),
                ))
                .is_err()
            {
                return;
            }
        }
    })
}

#[test]
fn the_handshake_completes_and_records_the_negotiated_protocol_version() {
    let (to_client, pipe) = testing::Pipe::new();
    let (sink, from_client) = testing::Sink::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let server = fake_server(to_client, from_client, "0.4.1", seen.clone());

    let client = Client::new(Box::new(pipe), Box::new(sink), Arc::new(IgnoreServer));
    let (version, compatibility) =
        launcher::handshake(&client, std::path::Path::new("/w")).expect("handshake");
    assert_eq!(version, "0.4.1");
    assert_eq!(compatibility, ProtocolCompatibility::Tested);

    let sent = client.sent_methods();
    assert_eq!(
        sent,
        [
            "initialize",
            "initialized",
            "typeServer/getSupportedProtocolVersion"
        ]
    );
    for forbidden in protocol::FORBIDDEN_SYNC_METHODS {
        assert!(
            !sent.contains(&forbidden.to_owned()),
            "{forbidden} would install an editor overlay over Workspace truth"
        );
    }
    client.close();
    drop(server);
    assert!(!seen.lock().expect("lock").is_empty());
}

#[test]
fn an_unknown_protocol_version_degrades_instead_of_being_parsed() {
    let (to_client, pipe) = testing::Pipe::new();
    let (sink, from_client) = testing::Sink::new();
    let server = fake_server(
        to_client,
        from_client,
        "0.5.0",
        Arc::new(Mutex::new(Vec::new())),
    );

    let client = Client::new(Box::new(pipe), Box::new(sink), Arc::new(IgnoreServer));
    let error = launcher::handshake(&client, std::path::Path::new("/w")).expect_err("refused");
    assert!(
        error.to_string().contains("0.5.0") && error.to_string().contains("degrade"),
        "unexpected: {error}"
    );
    client.close();
    drop(server);
}

#[test]
fn a_malformed_backend_message_does_not_corrupt_the_host() {
    let (host, to_client) = detached_host();
    let mut framed = Vec::new();
    write!(framed, "Content-Length: 10\r\n\r\n{{not json}}").expect("frame");
    to_client.send(framed).expect("send");
    thread::sleep(Duration::from_millis(50));
    assert_eq!(
        host.health(),
        HostHealth::Healthy,
        "an unreadable frame is skipped, not fatal"
    );
}

#[test]
fn watched_file_changes_are_the_only_synchronization_emitted() {
    let fixture = Fixture::create("watched");
    let backend = ScriptedBackend::new();
    adapter::notify_watched_files(
        &backend,
        vec![WatchedChange {
            uri: fixture.uri("pkg/impl.py"),
            kind: WatchedChangeKind::Changed,
        }],
    )
    .expect("notified");
    let calls = backend.calls();
    assert!(matches!(
        calls.as_slice(),
        [PythonRequest::WatchedFilesChanged { .. }]
    ));
    let (method, _) = calls[0].wire();
    assert_eq!(method, "workspace/didChangeWatchedFiles");
}

// ---------------------------------------------------------------------
// Snapshot discipline
// ---------------------------------------------------------------------

#[test]
fn every_query_in_a_batch_carries_one_coherent_snapshot() {
    let fixture = Fixture::create("coherent");
    let backend = impl_backend(&fixture);
    let produced = resolve(&fixture, &backend, "pkg/impl.py", &[]);
    assert!(!produced.evidence.is_empty());

    let snapshots = backend.snapshots_seen();
    assert!(
        snapshots.len() >= 3,
        "several snapshot-carrying queries ran"
    );
    assert!(
        snapshots.windows(2).all(|pair| pair[0] == pair[1]),
        "one candidate must not mix snapshots: {snapshots:?}"
    );
}

#[test]
fn a_stale_snapshot_restarts_the_whole_batch_rather_than_keeping_half() {
    let fixture = Fixture::create("stale");
    // The first snapshot-carrying query is refused, so the batch that
    // had already started is thrown away entirely.
    let backend = impl_backend(&fixture).with_stale_budget(1);
    let produced = resolve(&fixture, &backend, "pkg/impl.py", &[]);

    let snapshots = backend.snapshots_seen();
    let distinct: std::collections::BTreeSet<u64> = snapshots.iter().copied().collect();
    assert_eq!(distinct.len(), 2, "the retry ran on a new snapshot");
    // The surviving evidence is all from the second, coherent attempt.
    let last = *snapshots.last().expect("queries ran");
    assert!(
        snapshots.iter().filter(|value| **value == last).count() >= 3,
        "the whole batch re-ran on the fresh snapshot"
    );
    assert!(!produced.evidence.is_empty());
}

#[test]
fn snapshot_retry_is_bounded_and_exhaustion_publishes_nothing() {
    let fixture = Fixture::create("exhausted");
    let backend = impl_backend(&fixture).with_stale_budget(u32::MAX);

    let error = refresh(&fixture, &backend, "pkg/impl.py").expect_err("no publication");
    let PythonSemanticError::Backend(BatchError::RetriesExhausted { attempts }) = error else {
        panic!("expected bounded retry exhaustion, got {error}");
    };
    assert_eq!(attempts, BatchPolicy::default().max_attempts);

    // Nothing was published, and a retryable failure is not zero
    // semantic results.
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let status = index.status(&context().context_key()).expect("status");
    assert_eq!(status.state, SemanticState::None);
    assert_eq!(count(&fixture, "semantic_publication"), 0);
    assert_eq!(count(&fixture, "semantic_evidence"), 0);
}

#[test]
fn the_backend_snapshot_never_becomes_canonical_identity() {
    let first = Fixture::create("snapshot-a");
    refresh(&first, &impl_backend(&first), "pkg/impl.py").expect("published");
    let second = Fixture::create("snapshot-b");
    // A different backend snapshot number for the identical Workspace.
    let backend = impl_backend(&second);
    for _ in 0..5 {
        let _ = backend.call(&PythonRequest::Snapshot);
    }
    refresh(&second, &backend, "pkg/impl.py").expect("published");

    let read = |fixture: &Fixture| {
        let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
        let status = index.status(&context().context_key()).expect("status");
        let basis = status.basis.expect("published basis");
        (
            basis.config_fingerprint,
            basis.environment_fingerprint,
            basis.inventory_fingerprint,
        )
    };
    assert_eq!(
        read(&first),
        read(&second),
        "the basis is the same whatever the backend numbered its snapshot"
    );
}

// ---------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------

#[test]
fn an_internal_target_resolves_through_resource_identity_and_an_exact_span() {
    let fixture = Fixture::create("internal");
    let produced = resolve(&fixture, &impl_backend(&fixture), "pkg/impl.py", &[]);

    let call = produced
        .evidence
        .iter()
        .find(|item| {
            item.relation_kind == Some(RelationKind::Calls)
                && item.occurrence.is_some_and(|site| site.start_byte == 159)
        })
        .expect("the x.run(1) call site");

    // Base.run, not Other.run in twin.py, and not "some symbol named
    // run": the span decided it.
    let base_run = base_run_symbol(&fixture);
    assert_eq!(
        call.outcome,
        SemanticOutcome::Resolved {
            target: GraphEndpoint::Symbol(base_run)
        }
    );
    assert_eq!(call.capability, SemanticCapability::CallsIntraFile);
    assert_eq!(
        call.dispatch,
        Dispatch::Unknown,
        "a member call binds through the receiver's declared type; a subclass may still take over"
    );
    assert_eq!(call.support, Support::Supported);
}

fn base_run_symbol(fixture: &Fixture) -> brainprint_core::SymbolId {
    crate::symbol::SymbolStore::open(&fixture.db_path())
        .expect("index.db")
        .list_for_resource(fixture.resource("pkg/base.py").id)
        .expect("symbols")
        .into_iter()
        .find(|symbol| symbol.name == "run")
        .expect("Base.run")
        .id
}

#[test]
fn a_definition_is_matched_by_span_and_never_by_name() {
    let fixture = Fixture::create("byspan");
    let impl_uri = fixture.uri("pkg/impl.py");
    // The identical question, answered with the *other* file's `run`.
    let backend = impl_backend(&fixture).with_definition(
        &impl_uri,
        last_character(&fixture, "pkg/impl.py", "x.run", 0),
        vec![fixture.location("pkg/twin.py", "run", 0)],
    );
    let produced = resolve(&fixture, &backend, "pkg/impl.py", &[]);

    let twin_run = crate::symbol::SymbolStore::open(&fixture.db_path())
        .expect("index.db")
        .list_for_resource(fixture.resource("pkg/twin.py").id)
        .expect("symbols")
        .into_iter()
        .find(|symbol| symbol.name == "run")
        .expect("Other.run")
        .id;

    let call = produced
        .evidence
        .iter()
        .find(|item| item.occurrence.is_some_and(|site| site.start_byte == 159))
        .expect("the call site");
    assert_eq!(
        call.outcome,
        SemanticOutcome::Resolved {
            target: GraphEndpoint::Symbol(twin_run)
        },
        "two Symbols are named `run`; only the answered span picks one"
    );
    assert_ne!(twin_run, base_run_symbol(&fixture));
}

#[test]
fn several_distinct_internal_targets_stay_candidates_rather_than_a_guess() {
    let fixture = Fixture::create("ambiguous");
    let impl_uri = fixture.uri("pkg/impl.py");
    let backend = impl_backend(&fixture).with_definition(
        &impl_uri,
        last_character(&fixture, "pkg/impl.py", "x.run", 0),
        vec![
            fixture.location("pkg/base.py", "run", 0),
            fixture.location("pkg/twin.py", "run", 0),
        ],
    );
    let produced = resolve(&fixture, &backend, "pkg/impl.py", &[]);

    let call = produced
        .evidence
        .iter()
        .find(|item| item.occurrence.is_some_and(|site| site.start_byte == 159))
        .expect("the call site");
    let SemanticOutcome::Candidates { targets } = &call.outcome else {
        panic!("two real targets and nothing choosing: {:?}", call.outcome);
    };
    assert_eq!(targets.len(), 2);
}

#[test]
fn a_dependency_target_becomes_an_external_entity_and_no_resource_row() {
    let fixture = Fixture::create("external");
    let resources_before = count(&fixture, "resource");
    let produced = resolve(&fixture, &impl_backend(&fixture), "pkg/impl.py", &[]);

    let call = produced
        .evidence
        .iter()
        .find(|item| item.occurrence.is_some_and(|site| site.start_byte == 109))
        .expect("the json.dumps call site");
    let SemanticOutcome::Resolved {
        target: GraphEndpoint::External(entity),
    } = &call.outcome
    else {
        panic!("expected one external target, got {:?}", call.outcome);
    };
    // The stub and the implementation are the same module, so two
    // answers collapse to one identity instead of becoming candidates.
    assert_eq!(entity.package_identity, "json");
    assert_eq!(entity.module_path.as_deref(), Some("json"));
    assert_eq!(entity.symbol_name.as_deref(), Some("dumps"));
    assert_eq!(entity.kind, adapter::EXTERNAL_SYMBOL);
    assert_eq!(
        entity.declaration_locator, None,
        "no machine path enters canonical identity"
    );
    assert_eq!(call.capability, SemanticCapability::CallsCrossFile);
    assert_eq!(
        count(&fixture, "resource"),
        resources_before,
        "a dependency file never becomes an internal Resource"
    );
}

#[test]
fn absolute_relative_and_external_imports_all_normalize() {
    let fixture = Fixture::create("imports");
    let imports_uri = fixture.uri("pkg/imports.py");
    let text = fixture.text("pkg/imports.py");
    let site = |needle: &str| {
        let start = text.find(needle).expect("import site");
        OccurrenceRef {
            kind: OccurrenceKind::ImportSite,
            start_byte: start,
            end_byte: start + needle.len(),
        }
    };
    let gap = |occurrence: OccurrenceRef, name: &str| PersistedUnresolved {
        occurrence,
        intended: IntendedRelation::Known(RelationKind::Imports),
        lookup_name: name.to_owned(),
        module_hint: None,
        reason: UnresolvedReason::ImportTargetUnresolved,
        candidate_truncated: false,
        candidates: Vec::new(),
        resolution_context_key: None,
    };

    let backend = ScriptedBackend::new()
        .with_search_paths(vec![protocol::path_to_uri(std::path::Path::new(
            "/venv/site-packages",
        ))])
        .with_import(
            &imports_uri,
            0,
            &["pkg", "base"],
            Some(&fixture.uri("pkg/base.py")),
        )
        .with_import(
            &imports_uri,
            1,
            &["base"],
            Some(&fixture.uri("pkg/base.py")),
        )
        .with_import(
            &imports_uri,
            2,
            &["outside"],
            Some(&protocol::path_to_uri(std::path::Path::new(
                "/venv/site-packages/outside/__init__.py",
            ))),
        );

    let produced = resolve(
        &fixture,
        &backend,
        "pkg/imports.py",
        &[
            gap(site("pkg.base"), "pkg.base"),
            gap(site(".base"), "base"),
            gap(site("..outside"), "outside"),
        ],
    );

    let base = GraphEndpoint::Resource(fixture.resource("pkg/base.py").id);
    let by_span = |start: usize| {
        produced
            .evidence
            .iter()
            .find(|item| item.occurrence.is_some_and(|site| site.start_byte == start))
            .unwrap_or_else(|| panic!("evidence for the site at {start}"))
            .clone()
    };

    let absolute = by_span(site("pkg.base").start_byte);
    assert_eq!(
        absolute.outcome,
        SemanticOutcome::Resolved {
            target: base.clone()
        }
    );
    assert_eq!(absolute.capability, SemanticCapability::ImportBinding);
    assert_eq!(absolute.relation_kind, Some(RelationKind::Imports));

    let relative = by_span(site(".base").start_byte);
    assert_eq!(relative.outcome, SemanticOutcome::Resolved { target: base });

    let external = by_span(site("..outside").start_byte);
    let SemanticOutcome::Resolved {
        target: GraphEndpoint::External(entity),
    } = &external.outcome
    else {
        panic!("expected an external module, got {:?}", external.outcome);
    };
    assert_eq!(entity.package_identity, "outside");
    assert_eq!(entity.kind, adapter::EXTERNAL_MODULE);
    assert_eq!(
        external.capability,
        SemanticCapability::ExternalSymbolResolution
    );
}

#[test]
fn declared_and_computed_types_resolve_and_a_bare_display_name_does_not() {
    let fixture = Fixture::create("types");
    let produced = resolve(&fixture, &impl_backend(&fixture), "pkg/impl.py", &[]);
    let int_site = fixture.text("pkg/impl.py").find("int").expect("int");
    let type_evidence = produced
        .evidence
        .iter()
        .find(|item| {
            item.occurrence
                .is_some_and(|site| site.start_byte == int_site)
        })
        .expect("the int type site");
    assert_eq!(type_evidence.capability, SemanticCapability::TypeResolution);
    assert!(matches!(
        type_evidence.outcome,
        SemanticOutcome::Resolved {
            target: GraphEndpoint::External(_)
        }
    ));

    // A type that names itself and declares nothing is not a target.
    // Resolving "Base" by name is exactly the guess this tier refuses.
    let impl_uri = fixture.uri("pkg/impl.py");
    let nameless = impl_backend(&fixture).with_type(
        &impl_uri,
        fixture.range("pkg/impl.py", "int", 0),
        Some(TypeAnswer {
            declaration: None,
            module_name: None,
            module_uri: None,
        }),
    );
    let produced = resolve(&fixture, &nameless, "pkg/impl.py", &[]);
    let type_evidence = produced
        .evidence
        .iter()
        .find(|item| {
            item.occurrence
                .is_some_and(|site| site.start_byte == int_site)
        })
        .expect("the int type site");
    assert!(matches!(
        type_evidence.outcome,
        SemanticOutcome::Unresolved { .. }
    ));
}

#[test]
fn a_non_ascii_source_resolves_at_the_right_token() {
    let fixture = Fixture::create("nonascii");
    let uri = fixture.uri("pkg/unicode_case.py");
    let backend = ScriptedBackend::new()
        .with_search_paths(Vec::new())
        .with_definition(
            &uri,
            last_character(&fixture, "pkg/unicode_case.py", "x.run", 0),
            vec![fixture.location("pkg/base.py", "run", 0)],
        );
    let produced = resolve(&fixture, &backend, "pkg/unicode_case.py", &[]);

    let text = fixture.text("pkg/unicode_case.py");
    let call_site = text.find("x.run").expect("call site");
    let call = produced
        .evidence
        .iter()
        .find(|item| {
            item.occurrence
                .is_some_and(|site| site.start_byte == call_site)
        })
        .expect("the call site after non-ASCII source");
    assert_eq!(
        call.outcome,
        SemanticOutcome::Resolved {
            target: GraphEndpoint::Symbol(base_run_symbol(&fixture))
        }
    );

    // The position asked about is a UTF-16 offset, and it is not the
    // byte offset -- which on this line would be a different token.
    let line_start = text[..call_site].rfind('\n').map_or(0, |index| index + 1);
    let expected_line = u32::try_from(text[..call_site].matches('\n').count()).expect("fits");
    let expected_character = u32::try_from(
        text[line_start..call_site + "x.run".len() - 1]
            .encode_utf16()
            .count(),
    )
    .expect("fits");
    let asked: Vec<Position> = backend
        .calls()
        .into_iter()
        .filter_map(|request| match request {
            PythonRequest::Definition { position, .. } => Some(position),
            _ => None,
        })
        .collect();
    assert!(
        asked.contains(&Position::new(expected_line, expected_character)),
        "asked {asked:?}, wanted the UTF-16 offset {expected_character} on line {expected_line}"
    );
    // And on a line that actually has non-ASCII before the token --
    // `return 변수 + str(value)` -- the two conventions disagree, so
    // the asked offset proves which one reached the backend.
    let store = GraphStore::open(&fixture.db_path()).expect("index.db");
    let after_hangul = list_occurrences_for_resource(
        store.connection(),
        fixture.resource("pkg/unicode_case.py").id,
    )
    .expect("occurrences")
    .into_iter()
    .find(|occurrence| {
        occurrence.kind == OccurrenceKind::CallSite
            && text[occurrence.span.start_byte..occurrence.span.end_byte] == *"str"
    })
    .expect("the str(...) call after the Hangul");
    let site_line_start = text[..after_hangul.span.start_byte]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let site_line =
        u32::try_from(text[..after_hangul.span.start_byte].matches('\n').count()).expect("fits");
    let last = after_hangul.span.end_byte - 1;
    let utf16 = u32::try_from(text[site_line_start..last].encode_utf16().count()).expect("fits");
    let bytes = u32::try_from(last - site_line_start).expect("fits");
    assert_ne!(utf16, bytes, "this line discriminates the conventions");
    assert!(
        asked.contains(&Position::new(site_line, utf16)),
        "asked {asked:?}, wanted UTF-16 {utf16} on line {site_line}"
    );
    assert!(
        !asked.contains(&Position::new(site_line, bytes)),
        "a byte offset reached the backend"
    );
}

// ---------------------------------------------------------------------
// Capability and boundary
// ---------------------------------------------------------------------

#[test]
fn the_capability_report_states_what_this_task_implements() {
    let report = capability_report(&context());
    for supported in [
        SemanticCapability::ImportBinding,
        SemanticCapability::ExternalSymbolResolution,
        SemanticCapability::SymbolDefinition,
        SemanticCapability::References,
        SemanticCapability::CallsIntraFile,
        SemanticCapability::CallsCrossFile,
    ] {
        assert_eq!(
            report.support(supported),
            Support::Supported,
            "{supported:?}"
        );
    }
    // Declared and computed types resolve; the expected type does not
    // always, so the capability is partial and not rounded up.
    assert_eq!(
        report.support(SemanticCapability::TypeResolution),
        Support::Partial
    );
    assert_eq!(EXPECTED_TYPE_SUPPORT, Support::Partial);

    // Derived from proven inheritance, and partial for reasons the
    // constants name: an unanchorable qualified base, decorator forms
    // the Symbol model does not distinguish, and same-depth ancestors
    // whose base-list order the graph does not record.
    assert_eq!(
        report.support(SemanticCapability::Inheritance),
        Support::Partial
    );
    assert_eq!(
        report.support(SemanticCapability::Overrides),
        Support::Partial
    );
    assert_eq!(INHERITANCE_SUPPORT, Support::Partial);
    assert_eq!(OVERRIDES_SUPPORT, Support::Partial);

    // Python has no implements clause, and matching members is duck
    // typing rather than evidence.
    for unsupported in [
        SemanticCapability::Implements,
        SemanticCapability::ImplementationTarget,
    ] {
        assert_eq!(
            report.support(unsupported),
            Support::Unsupported,
            "{unsupported:?}"
        );
    }
}

#[test]
fn a_base_list_gap_resolves_and_an_unwritable_kind_is_deferred() {
    let fixture = Fixture::create("deferred");
    let text = fixture.text("pkg/impl.py");
    let base_site = text.rfind("Base").expect("a Base type site");
    let site = OccurrenceRef {
        kind: OccurrenceKind::TypeSite,
        start_byte: base_site,
        end_byte: base_site + 4,
    };
    let gap = |intended| PersistedUnresolved {
        occurrence: site,
        intended,
        lookup_name: "Base".to_owned(),
        module_hint: None,
        reason: UnresolvedReason::TypeSemanticsRequired,
        candidate_truncated: false,
        candidates: Vec::new(),
        resolution_context_key: None,
    };

    // A base list is inheritance in Python whichever way I3 labelled
    // it: the language has no implements clause.
    for intended in [
        IntendedRelation::Known(RelationKind::Extends),
        IntendedRelation::Inheritance,
    ] {
        let uri = fixture.uri("pkg/impl.py");
        let backend = impl_backend(&fixture).with_definition(
            &uri,
            last_character(&fixture, "pkg/impl.py", "Base", 2),
            vec![fixture.location("pkg/base.py", "Base", 0)],
        );
        let produced = resolve(&fixture, &backend, "pkg/impl.py", &[gap(intended)]);
        let resolved = produced
            .evidence
            .iter()
            .find(|item| item.occurrence == Some(site))
            .expect("the base site was answered");
        assert_eq!(resolved.relation_kind, Some(RelationKind::Extends));
        assert_eq!(resolved.capability, SemanticCapability::Inheritance);
        assert!(produced.deferred.is_empty());
    }

    // A relation nobody writes at a site has nowhere to anchor as a
    // gap, and is answered by derivation instead.
    let produced = resolve(
        &fixture,
        &impl_backend(&fixture),
        "pkg/impl.py",
        &[gap(IntendedRelation::Known(RelationKind::Overrides))],
    );
    assert_eq!(produced.deferred.len(), 1);
    assert!(
        produced
            .evidence
            .iter()
            .all(|item| item.relation_kind != Some(RelationKind::Overrides)),
        "no OVERRIDES comes from a gap"
    );
}

#[test]
fn call_hierarchy_normalizes_only_onto_occurrences_that_exist() {
    let fixture = Fixture::create("hierarchy");
    let owner = fixture.resource("pkg/impl.py");
    let text = fixture.text("pkg/impl.py");
    let store = GraphStore::open(&fixture.db_path()).expect("index.db");
    let occurrences =
        list_occurrences_for_resource(store.connection(), owner.id).expect("occurrences");
    let context = context();
    let key = context.context_key();
    let owner_uri = fixture.uri("pkg/impl.py");
    let request = ResourceRequest {
        owner: &owner,
        owner_text: &text,
        gaps: &[],
        occurrences: &occurrences,
        generation_id: 1,
        analysis_profile_id: 1,
        context_key: &key,
    };

    let call_site = fixture.range("pkg/impl.py", "x.run", 0);
    let nowhere = Range::new(Position::new(0, 0), Position::new(0, 4));
    let target = GraphEndpoint::Symbol(base_run_symbol(&fixture));
    let evidence = adapter::normalize_incoming_calls(
        &[
            protocol::IncomingCall {
                from: fixture.location("pkg/impl.py", "call", 0),
                from_ranges: vec![call_site, call_site, nowhere],
            },
            protocol::IncomingCall {
                from: fixture.location("pkg/unicode_case.py", "x.run", 0),
                from_ranges: vec![fixture.range("pkg/unicode_case.py", "x.run", 0)],
            },
        ],
        &target,
        &request,
        &owner_uri,
    );

    // Two identical call-site ranges are one piece of evidence, a range
    // with no Occurrence under it contributes nothing, and another
    // Resource's sites belong to that Resource's own contribution.
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].relation_kind, Some(RelationKind::Calls));
    assert_eq!(
        evidence[0].occurrence.expect("anchored").start_byte,
        text.find("x.run").expect("call site")
    );
}

#[test]
fn normalized_evidence_carries_no_backend_identity() {
    let fixture = Fixture::create("noidentity");
    let produced = resolve(&fixture, &impl_backend(&fixture), "pkg/impl.py", &[]);
    assert!(!produced.evidence.is_empty());
    let rendered = format!("{:?}", produced.evidence);
    for leaked in ["file://", "typeServer/", "textDocument/", "snapshot"] {
        assert!(
            !rendered.contains(leaked),
            "{leaked} must not survive normalization: {rendered}"
        );
    }
}

// ---------------------------------------------------------------------
// Publication and merge
// ---------------------------------------------------------------------

#[test]
fn a_refresh_publishes_a_complete_basis_and_merges_into_the_one_graph() {
    let fixture = Fixture::create("publish");
    let outcome = refresh(&fixture, &impl_backend(&fixture), "pkg/impl.py").expect("published");
    assert!(outcome.evidence_count > 0);

    let basis = &outcome.publication.basis;
    assert!(
        basis.inventory_fingerprint.is_some(),
        "the Pyright snapshot is program-wide, so the module set is part of the basis"
    );
    assert!(!basis.config_fingerprint.is_empty());
    assert!(!basis.environment_fingerprint.is_empty());
    assert!(
        basis
            .sources
            .contains_key(&fixture.resource("pkg/impl.py").id),
        "the Resource the analysis read is in its basis"
    );
    assert!(
        basis
            .sources
            .contains_key(&fixture.resource("pkg/base.py").id),
        "so is the Resource a target was matched in"
    );

    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    assert_eq!(
        index
            .status(&context().context_key())
            .expect("status")
            .state,
        SemanticState::Current
    );

    // The merge reached the one canonical graph: the previously
    // unresolved call now answers through the ordinary relation API,
    // with no Python-specific query surface anywhere.
    assert!(outcome.merged.gaps_resolved > 0);
    let relations = RelationIndex::open(&fixture.db_path()).expect("index.db");
    let callers = relations
        .callers(&GraphEndpoint::Symbol(base_run_symbol(&fixture)))
        .expect("callers");
    assert!(
        callers.confirmed_count() > 0,
        "`callers` answers from the merged graph without a semantic API"
    );
}

#[test]
fn an_unanchored_backend_fact_is_rejected_rather_than_inventing_an_occurrence() {
    let fixture = Fixture::create("unanchored");
    // A gap whose span no Occurrence covers.
    let phantom = PersistedUnresolved {
        occurrence: OccurrenceRef {
            kind: OccurrenceKind::CallSite,
            start_byte: 3,
            end_byte: 7,
        },
        intended: IntendedRelation::Known(RelationKind::Calls),
        lookup_name: "ghost".to_owned(),
        module_hint: None,
        reason: UnresolvedReason::NoStructuralBinding,
        candidate_truncated: false,
        candidates: Vec::new(),
        resolution_context_key: None,
    };
    let uri = fixture.uri("pkg/impl.py");
    let backend = impl_backend(&fixture).with_definition(
        &uri,
        Position::new(0, 9),
        vec![fixture.location("pkg/base.py", "run", 0)],
    );

    let occurrences_before = count(&fixture, "occurrence");
    let produced = resolve(&fixture, &backend, "pkg/impl.py", &[phantom]);
    assert!(
        produced
            .evidence
            .iter()
            .any(|item| item.occurrence.is_some_and(|site| site.start_byte == 3)),
        "the adapter still reports it; the merge is what refuses it"
    );
    // The refresh path runs the same evidence through merge, which
    // rejects an unanchored fact instead of creating a span.
    refresh(&fixture, &impl_backend(&fixture), "pkg/impl.py").expect("published");
    assert_eq!(count(&fixture, "occurrence"), occurrences_before);
}

#[test]
fn a_source_change_while_the_backend_thinks_rejects_the_candidate() {
    let fixture = Fixture::create("moved");
    let owner = fixture.resource("pkg/impl.py").id;
    let db_path = fixture.db_path();

    // The Workspace moves on *after* the analysis read the Resource and
    // before the candidate is published -- the window the publication
    // transaction exists to close.
    let backend = impl_backend(&fixture).with_hook(move |request| {
        if matches!(request, PythonRequest::Definition { .. }) {
            let store = GraphStore::open(&db_path).expect("index.db");
            store
                .connection()
                .execute(
                    "UPDATE resource SET resource_revision = 'moved-on' WHERE uid = ?1",
                    rusqlite::params![owner.to_bytes().to_vec()],
                )
                .expect("move the revision");
        }
    });

    let error = refresh(&fixture, &backend, "pkg/impl.py").expect_err("obsolete");
    let PythonSemanticError::Index(SemanticIndexError::Obsolete { reason, .. }) = &error else {
        panic!("expected an obsolete basis, got {error}");
    };
    assert!(
        matches!(
            reason,
            crate::semantic_index::ObsoleteReason::SourceRevisionMoved { .. }
        ),
        "unexpected: {reason}"
    );
    assert_eq!(count(&fixture, "semantic_publication"), 0);
    assert_eq!(count(&fixture, "semantic_evidence"), 0);
}

#[test]
fn a_semantic_merge_is_idempotent() {
    let fixture = Fixture::create("idempotent");
    let first = refresh(&fixture, &impl_backend(&fixture), "pkg/impl.py").expect("published");
    let relations_after_first = count(&fixture, "relation");
    let evidence_after_first = count(&fixture, "semantic_evidence");
    assert!(first.merged.gaps_resolved > 0);

    let second = refresh(&fixture, &impl_backend(&fixture), "pkg/impl.py").expect("republished");
    assert_eq!(count(&fixture, "relation"), relations_after_first);
    assert_eq!(count(&fixture, "semantic_evidence"), evidence_after_first);
    assert_eq!(
        second.merged.gaps_resolved, 0,
        "nothing was still open the second time"
    );
    assert_eq!(second.merged.relations_created, 0);
}

#[test]
fn a_backend_failure_publishes_nothing_and_keeps_structural_truth() {
    let fixture = Fixture::create("timeout");
    let relations_before = count(&fixture, "relation");
    let backend = ScriptedBackend::new().failing(RequestFailure::TimedOut {
        after: Duration::from_millis(50),
    });

    let error = refresh(&fixture, &backend, "pkg/impl.py").expect_err("no publication");
    assert!(matches!(
        error,
        PythonSemanticError::Backend(BatchError::Request(RequestFailure::TimedOut { .. }))
    ));
    assert_eq!(count(&fixture, "semantic_publication"), 0);
    assert_eq!(
        count(&fixture, "relation"),
        relations_before,
        "a timeout is not zero semantic results and not a loss of structural ones"
    );
    // And the structural gaps are still recorded, so coverage stays
    // honest rather than reading as "nothing to resolve".
    let store = GraphStore::open(&fixture.db_path()).expect("index.db");
    let gaps = list_unresolved_for_resource(store.connection(), fixture.resource("pkg/impl.py").id)
        .expect("gaps");
    assert!(!gaps.is_empty());
}

#[test]
fn cancelling_one_request_does_not_stop_the_shared_runtime() {
    let (host, _to_client) = detached_host();
    let cancel = CancelToken::new();
    cancel.cancel();
    let failure = host
        .call(&PythonRequest::Snapshot, &cancel)
        .expect_err("cancelled");
    assert!(failure.to_string().contains("cancelled"), "{failure}");
    assert_eq!(
        host.health(),
        HostHealth::Healthy,
        "one caller giving up never retires a shared backend"
    );
}

#[test]
fn no_dependency_source_or_raw_backend_payload_reaches_the_index() {
    let fixture = Fixture::create("nodump");
    refresh(&fixture, &impl_backend(&fixture), "pkg/impl.py").expect("published");

    let store = GraphStore::open(&fixture.db_path()).expect("index.db");
    let connection = store.connection();
    // No Resource outside the Workspace, and nothing named after a
    // dependency path.
    let outside: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM resource WHERE path_key LIKE '%site-packages%' \
             OR path_key LIKE '%typeshed%'",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(outside, 0);

    // The whole semantic contribution holds identity and provenance,
    // never a payload: every text column is short.
    let mut statement = connection
        .prepare("SELECT context_key, capability, proof_role FROM semantic_evidence")
        .expect("prepare");
    let rows: Vec<(String, String, String)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert!(!rows.is_empty());
    for (context_key, capability, role) in rows {
        assert!(context_key.starts_with("sha256-ac1:"));
        assert!(SemanticCapability::parse(&capability).is_ok());
        assert!(!role.is_empty());
    }
}

#[test]
fn identity_and_basis_inputs_move_only_when_their_inputs_do() {
    let fixture = Fixture::create("fingerprints");
    let store = GraphStore::open(&fixture.db_path()).expect("index.db");
    let connection = store.connection();

    let inventory = inventory_fingerprint(connection, ResourceLanguage::Python).expect("inventory");
    assert_eq!(
        inventory,
        inventory_fingerprint(connection, ResourceLanguage::Python).expect("again"),
        "deterministic"
    );
    // A new module changes the module set; editing one does not.
    connection
        .execute(
            "UPDATE resource SET path_key = 'pkg/renamed.py' WHERE path_key = 'pkg/twin.py'",
            [],
        )
        .expect("rename");
    assert_ne!(
        inventory,
        inventory_fingerprint(connection, ResourceLanguage::Python).expect("after rename")
    );

    let settings = PythonSettings {
        python_path: Some("/venv/bin/python".to_owned()),
        ..PythonSettings::default()
    };
    assert_ne!(
        environment_fingerprint(&settings),
        environment_fingerprint(&PythonSettings::default()),
        "the interpreter decides what an import resolves to"
    );
    let install = PyrightInstall {
        node: std::path::PathBuf::from("/usr/bin/node"),
        server_script: std::path::PathBuf::from("/opt/a/pyright-typeserver.js"),
        package_version: "1.1.414".to_owned(),
    };
    let elsewhere = PyrightInstall {
        server_script: std::path::PathBuf::from("/opt/b/pyright-typeserver.js"),
        ..install.clone()
    };
    assert_eq!(
        toolchain_identity(&install, &settings),
        toolchain_identity(&elsewhere, &settings),
        "where the type server is installed does not change what Python means"
    );
    assert_eq!(
        toolchain_identity(&install, &settings).backend_compatibility_class,
        PythonLauncher::compatibility_class()
    );
}

#[test]
fn config_basis_follows_the_config_file_and_the_settings() {
    let fixture = Fixture::create("config");
    let config_resource = fixture.resource("pkg/base.py");
    let empty = config_basis(&PythonSettings::default(), None);
    let with_file = config_basis(&PythonSettings::default(), Some(&config_resource));
    assert_ne!(empty.fingerprint(), with_file.fingerprint());

    let with_mode = config_basis(
        &PythonSettings {
            type_checking_mode: Some("strict".to_owned()),
            ..PythonSettings::default()
        },
        Some(&config_resource),
    );
    assert_ne!(with_file.fingerprint(), with_mode.fingerprint());
    // The interpreter belongs to the environment axis, not this one.
    let with_interpreter = config_basis(
        &PythonSettings {
            python_path: Some("/venv/bin/python".to_owned()),
            ..PythonSettings::default()
        },
        Some(&config_resource),
    );
    assert_eq!(with_file.fingerprint(), with_interpreter.fingerprint());
}

// ---------------------------------------------------------------------
// Inheritance and override derivation (#19 task 7)
// ---------------------------------------------------------------------

/// The Symbol with this qualified name, in this file.
fn symbol(fixture: &Fixture, rel: &str, qualified_name: &str) -> brainprint_core::SymbolId {
    crate::symbol::SymbolStore::open(&fixture.db_path())
        .expect("index.db")
        .list_for_resource(fixture.resource(rel).id)
        .expect("symbols")
        .into_iter()
        .find(|symbol| symbol.qualified_name == qualified_name)
        .unwrap_or_else(|| panic!("{qualified_name} is indexed in {rel}"))
        .id
}

/// Every OVERRIDES edge leaving a declaration.
fn overrides_of(fixture: &Fixture, from: brainprint_core::SymbolId) -> Vec<GraphEndpoint> {
    RelationIndex::open(&fixture.db_path())
        .expect("index.db")
        .outgoing(&GraphEndpoint::Symbol(from), &[RelationKind::Overrides])
        .expect("outgoing")
        .confirmed
        .into_iter()
        .map(|relation| relation.target)
        .collect()
}

#[test]
fn a_subclass_method_overrides_the_ancestor_member_its_inheritance_proves() {
    let fixture = Fixture::create("override");
    let outcome = refresh(&fixture, &impl_backend(&fixture), "pkg/impl.py").expect("published");

    let impl_run = symbol(&fixture, "pkg/impl.py", "Impl.run");
    let base_run = symbol(&fixture, "pkg/base.py", "Base.run");
    assert_eq!(
        overrides_of(&fixture, impl_run),
        vec![GraphEndpoint::Symbol(base_run)]
    );

    // Anchored on the overriding declaration's own name token, so a
    // caller gets the exact current source and not a line number.
    let text = fixture.text("pkg/impl.py");
    let evidence = outcome
        .report
        .iter()
        .find(|line| line.starts_with("Overrides"))
        .expect("the derivation is reported");
    let start = text.find("    def run").expect("the declaration") + 8;
    assert!(
        evidence.contains(&format!("@{start}..{}", start + 3)),
        "unexpected anchor: {evidence}"
    );
    assert_eq!(&text[start..start + 3], "run");

    // The ancestor's Resource is part of the proof, so it is part of
    // the basis: `base.py` moving must make this publication stale.
    assert!(
        outcome
            .publication
            .basis
            .sources
            .contains_key(&fixture.resource("pkg/base.py").id),
        "the ancestor declaration is a dependency of the derived edge"
    );
}

#[test]
fn an_unrelated_method_of_the_same_name_never_becomes_an_override() {
    let fixture = Fixture::create("nottrap");
    for rel in ["pkg/impl.py", "pkg/twin.py", "pkg/inherit.py"] {
        refresh(&fixture, &impl_backend(&fixture), rel).expect("published");
    }
    // Three classes declare `run` and only one of them inherits it.
    for (rel, name) in [
        ("pkg/twin.py", "Other.run"),
        ("pkg/inherit.py", "Unrelated.run"),
        ("pkg/inherit.py", "Mixin.run"),
    ] {
        assert!(
            overrides_of(&fixture, symbol(&fixture, rel, name)).is_empty(),
            "{name} overrides nothing it is related to"
        );
    }
    assert_eq!(
        overrides_of(&fixture, symbol(&fixture, "pkg/impl.py", "Impl.run")).len(),
        1,
        "the one that does inherit still resolves"
    );
}

#[test]
fn multiple_inheritance_resolves_a_unique_member_and_refuses_an_ambiguous_one() {
    let fixture = Fixture::create("mro");
    let outcome = refresh(&fixture, &impl_backend(&fixture), "pkg/inherit.py").expect("published");

    // `OnlyOne(Base, Mixin).only_mixin`: only Mixin declares it, so
    // the base-list order never comes into it.
    assert_eq!(
        overrides_of(
            &fixture,
            symbol(&fixture, "pkg/inherit.py", "OnlyOne.only_mixin")
        ),
        vec![GraphEndpoint::Symbol(symbol(
            &fixture,
            "pkg/inherit.py",
            "Mixin.only_mixin"
        ))]
    );

    // `Multi(Base, Mixin).run`: both declare it. Python's MRO picks
    // the first base; a canonical EXTENDS edge is set membership and
    // carries no order, so nothing here chooses.
    let multi_run = symbol(&fixture, "pkg/inherit.py", "Multi.run");
    assert!(overrides_of(&fixture, multi_run).is_empty());
    let ambiguous = outcome
        .unproven_overrides
        .iter()
        .find(|unproven| unproven.method == multi_run)
        .expect("the ambiguity is reported, not silently dropped");
    assert_eq!(
        ambiguous.reason,
        UnprovenReason::AmbiguousAncestors { candidates: 2 }
    );
}

#[test]
fn an_external_base_makes_a_missing_member_opaque_rather_than_negative() {
    let fixture = Fixture::create("opaque");
    let outcome = refresh(&fixture, &impl_backend(&fixture), "pkg/shapes.py").expect("published");

    // `Runner(Protocol)`: the base is outside the Workspace and is not
    // indexed, so "nothing declares run" would be a claim the evidence
    // does not support.
    let runner_run = symbol(&fixture, "pkg/shapes.py", "Runner.run");
    assert!(overrides_of(&fixture, runner_run).is_empty());
    assert!(
        outcome
            .unproven_overrides
            .iter()
            .any(|unproven| unproven.method == runner_run
                && unproven.reason == UnprovenReason::OpaqueAncestor)
    );
}

#[test]
fn matching_members_never_infer_protocol_conformance() {
    let fixture = Fixture::create("protocol");
    for rel in ["pkg/shapes.py", "pkg/impl.py"] {
        refresh(&fixture, &impl_backend(&fixture), rel).expect("published");
    }
    // `DuckTyped` has exactly `Runner`'s member shape and says nothing
    // about it. Duck typing is not evidence.
    let relations = RelationIndex::open(&fixture.db_path()).expect("index.db");
    let duck = symbol(&fixture, "pkg/shapes.py", "DuckTyped");
    assert!(
        relations
            .outgoing(&GraphEndpoint::Symbol(duck), &[RelationKind::Implements])
            .expect("outgoing")
            .confirmed
            .is_empty()
    );
    assert!(overrides_of(&fixture, symbol(&fixture, "pkg/shapes.py", "DuckTyped.run")).is_empty());
    assert_eq!(
        count(&fixture, "relation WHERE kind = 'IMPLEMENTS'"),
        0,
        "no IMPLEMENTS is manufactured anywhere"
    );
    assert_eq!(
        capability_report(&context()).support(SemanticCapability::Implements),
        Support::Unsupported
    );
}

#[test]
fn an_abstract_base_gives_its_concrete_members_overrides() {
    let fixture = Fixture::create("abstract");
    refresh(&fixture, &impl_backend(&fixture), "pkg/shapes.py").expect("published");

    // Including the decorated forms. Python writes classmethod,
    // staticmethod and property as decorators and I3 records all three
    // as METHOD, which is why the capability is PARTIAL -- the kinds
    // are not distinguished, so a mismatch between them is not caught.
    for member in ["compute", "build", "helper", "label"] {
        assert_eq!(
            overrides_of(
                &fixture,
                symbol(&fixture, "pkg/shapes.py", &format!("Concrete.{member}"))
            ),
            vec![GraphEndpoint::Symbol(symbol(
                &fixture,
                "pkg/shapes.py",
                &format!("Abstract.{member}")
            ))],
            "Concrete.{member}"
        );
    }
}

#[test]
fn a_qualified_base_keeps_an_exact_anchor_and_resolves_semantically() {
    let fixture = Fixture::create("qualified");
    let text = fixture.text("pkg/inherit.py");
    let written = "base.Base";
    let at = text.find(written).expect("the qualified base");
    let site = OccurrenceRef {
        kind: OccurrenceKind::TypeSite,
        start_byte: at,
        end_byte: at + written.len(),
    };

    // Structurally: an exact Occurrence over the whole written
    // expression, owned by the subclass, left open for semantics.
    let store = GraphStore::open(&fixture.db_path()).expect("index.db");
    let occurrence =
        list_occurrences_for_resource(store.connection(), fixture.resource("pkg/inherit.py").id)
            .expect("occurrences")
            .into_iter()
            .find(|occurrence| occurrence.span.start_byte == site.start_byte)
            .expect("the qualified base has a site");
    assert_eq!(occurrence.kind, OccurrenceKind::TypeSite);
    assert_eq!(occurrence.span.end_byte, site.end_byte);
    assert_eq!(
        occurrence.containing_symbol_id,
        Some(symbol(&fixture, "pkg/inherit.py", "Qualified")),
        "the subclass owns its own base list"
    );
    assert!(occurrence.relation_id.is_none(), "nothing is guessed");

    let gap =
        list_unresolved_for_resource(store.connection(), fixture.resource("pkg/inherit.py").id)
            .expect("gaps")
            .into_iter()
            .find(|gap| gap.occurrence == site)
            .expect("a semantic-required inheritance gap");
    assert_eq!(gap.intended, IntendedRelation::Known(RelationKind::Extends));
    assert_eq!(gap.reason, UnresolvedReason::TypeSemanticsRequired);
    drop(store);

    // Semantically: the backend proves the target and the edge is
    // sourced from the subclass Symbol.
    let uri = fixture.uri("pkg/inherit.py");
    let backend = impl_backend(&fixture).with_definition(
        &uri,
        last_character(&fixture, "pkg/inherit.py", written, 0),
        vec![fixture.location("pkg/base.py", "Base", 0)],
    );
    let outcome = refresh(&fixture, &backend, "pkg/inherit.py").expect("published");
    assert!(outcome.merged.gaps_resolved > 0);

    let qualified = symbol(&fixture, "pkg/inherit.py", "Qualified");
    let extends: Vec<GraphEndpoint> = RelationIndex::open(&fixture.db_path())
        .expect("index.db")
        .outgoing(&GraphEndpoint::Symbol(qualified), &[RelationKind::Extends])
        .expect("outgoing")
        .confirmed
        .into_iter()
        .map(|relation| relation.target)
        .collect();
    assert_eq!(
        extends,
        vec![GraphEndpoint::Symbol(symbol(
            &fixture,
            "pkg/base.py",
            "Base"
        ))]
    );

    // And the derivation that depends on it now works too.
    assert_eq!(
        overrides_of(
            &fixture,
            symbol(&fixture, "pkg/inherit.py", "Qualified.run")
        ),
        vec![GraphEndpoint::Symbol(symbol(
            &fixture,
            "pkg/base.py",
            "Base.run"
        ))]
    );
}

#[test]
fn a_qualified_base_to_a_dependency_normalizes_without_indexing_it() {
    let fixture = Fixture::create("abc");
    let resources_before = count(&fixture, "resource");
    let uri = fixture.uri("pkg/shapes.py");
    let backend = impl_backend(&fixture).with_definition(
        &uri,
        last_character(&fixture, "pkg/shapes.py", "abc.ABC", 0),
        vec![Location {
            uri: protocol::path_to_uri(std::path::Path::new("/typeshed/stdlib/abc.pyi")),
            range: Range::new(Position::new(20, 6), Position::new(20, 9)),
        }],
    );
    refresh(&fixture, &backend, "pkg/shapes.py").expect("published");

    let abstract_class = symbol(&fixture, "pkg/shapes.py", "Abstract");
    let extends = RelationIndex::open(&fixture.db_path())
        .expect("index.db")
        .outgoing(
            &GraphEndpoint::Symbol(abstract_class),
            &[RelationKind::Extends],
        )
        .expect("outgoing")
        .confirmed;
    assert_eq!(extends.len(), 1);
    let GraphEndpoint::External(entity) = &extends[0].target else {
        panic!(
            "a dependency base is an external identity: {:?}",
            extends[0]
        );
    };
    assert_eq!(entity.package_identity, "abc");
    assert_eq!(
        count(&fixture, "resource"),
        resources_before,
        "no dependency file becomes a Resource to hold the base"
    );
}

#[test]
fn a_qualified_base_without_a_backend_keeps_the_answer_incomplete() {
    let fixture = Fixture::create("falsezero");
    // No semantic publication at all: the structural tier alone.
    let store = GraphStore::open(&fixture.db_path()).expect("index.db");
    let gaps =
        list_unresolved_for_resource(store.connection(), fixture.resource("pkg/inherit.py").id)
            .expect("gaps");
    assert!(
        gaps.iter().any(
            |gap| gap.intended == IntendedRelation::Known(RelationKind::Extends)
                && gap.reason == UnresolvedReason::TypeSemanticsRequired
        ),
        "the inheritance site survives without a backend"
    );
    drop(store);

    // So an inheritance query over the subclass is incomplete rather
    // than a clean zero.
    let answer = RelationIndex::open(&fixture.db_path())
        .expect("index.db")
        .outgoing(
            &GraphEndpoint::Symbol(symbol(&fixture, "pkg/inherit.py", "Qualified")),
            &[RelationKind::Extends],
        )
        .expect("outgoing");
    assert_eq!(answer.confirmed_count(), 0);
    assert!(
        !answer.gaps.is_empty(),
        "zero confirmed with no gap would be the false zero this prevents"
    );
    assert!(
        !answer.coverage.limits().is_complete(),
        "coverage says the answer is not complete"
    );
}

#[test]
fn inheritance_is_sourced_from_the_subclass_and_replaced_on_re_extraction() {
    let fixture = Fixture::create("ownership");
    let resource_sourced = |fixture: &Fixture| -> i64 {
        GraphStore::open(&fixture.db_path())
            .expect("index.db")
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM relation \
                 JOIN graph_entity ON graph_entity.id = relation.source_entity_id \
                 WHERE relation.kind = 'EXTENDS' \
                   AND graph_entity.entity_kind = 'RESOURCE'",
                [],
                |row| row.get(0),
            )
            .expect("count")
    };
    assert_eq!(
        resource_sourced(&fixture),
        0,
        "no inheritance relation is sourced from a file"
    );
    assert!(
        RelationIndex::open(&fixture.db_path())
            .expect("index.db")
            .outgoing(
                &GraphEndpoint::Symbol(symbol(&fixture, "pkg/impl.py", "Impl")),
                &[RelationKind::Extends],
            )
            .expect("outgoing")
            .confirmed
            .iter()
            .any(|relation| relation.target
                == GraphEndpoint::Symbol(symbol(&fixture, "pkg/base.py", "Base")))
    );

    // Re-extracting the whole Workspace neither duplicates the edge
    // nor reintroduces a Resource-sourced one.
    let before = count(&fixture, "relation WHERE kind = 'EXTENDS'");
    crate::scan::BaselineScan::open(&fixture.db_path())
        .expect("index.db")
        .run_initial_scan(
            &fixture.root,
            &crate::config::WorkspaceConfig::default(),
            "workspace-rev-2",
        )
        .expect("re-extract");
    assert_eq!(count(&fixture, "relation WHERE kind = 'EXTENDS'"), before);
    assert_eq!(resource_sourced(&fixture), 0);
}

#[test]
fn a_base_interface_change_reaches_the_subclass_through_the_corrected_source() {
    let fixture = Fixture::create("baseimpact");
    let base = GraphEndpoint::Symbol(symbol(&fixture, "pkg/base.py", "Base"));
    let impact = crate::impact::ImpactTraversal::open(&fixture.db_path())
        .expect("index.db")
        .run(
            crate::impact::ImpactIntent::BaseInterfaceChange,
            &base,
            &crate::impact::Budget::default(),
        )
        .expect("impact");
    let subclass = GraphEndpoint::Symbol(symbol(&fixture, "pkg/impl.py", "Impl"));
    assert!(
        impact.nodes.iter().any(|node| node.endpoint == subclass),
        "a Resource-sourced EXTENDS could never have identified the subclass"
    );
}

#[test]
fn a_declared_override_with_no_provable_target_stays_an_honest_gap() {
    let fixture = Fixture::create("declared");
    let uri = fixture.uri("pkg/inherit.py");
    let text = fixture.text("pkg/inherit.py");

    // Resolve every `@override` the way the real backend does: into
    // typing's stub.
    let mut backend = impl_backend(&fixture).with_search_paths(vec![protocol::path_to_uri(
        std::path::Path::new("/typeshed/stdlib"),
    )]);
    let mut from = 0;
    while let Some(at) = text[from..].find("@override") {
        let site = from + at + 1;
        let line_start = text[..site].rfind('\n').map_or(0, |index| index + 1);
        let line = u32::try_from(text[..site].matches('\n').count()).expect("fits");
        let character = u32::try_from(
            text[line_start..site + "override".len() - 1]
                .chars()
                .count(),
        )
        .expect("fits");
        backend = backend.with_definition(
            &uri,
            Position::new(line, character),
            vec![Location {
                uri: protocol::path_to_uri(std::path::Path::new("/typeshed/stdlib/typing.pyi")),
                range: Range::new(Position::new(100, 4), Position::new(100, 12)),
            }],
        );
        from = site;
    }

    let outcome = refresh(&fixture, &backend, "pkg/inherit.py").expect("published");

    // `Declared(Base).run` claims an override and proves one.
    assert_eq!(
        overrides_of(&fixture, symbol(&fixture, "pkg/inherit.py", "Declared.run")).len(),
        1
    );
    // `Orphan.run` claims one and has no base at all. The claim does
    // not create the edge; it makes the silence worth reporting.
    let orphan = symbol(&fixture, "pkg/inherit.py", "Orphan.run");
    assert!(overrides_of(&fixture, orphan).is_empty());
    assert!(
        outcome
            .unproven_overrides
            .iter()
            .any(|unproven| unproven.method == orphan
                && unproven.reason == UnprovenReason::NoAncestorMember),
        "unproven: {:?}",
        outcome.unproven_overrides
    );
}

#[test]
fn a_call_site_keeps_its_own_relation_kind_and_is_not_also_a_reference() {
    let fixture = Fixture::create("kinds");
    let produced = resolve(&fixture, &impl_backend(&fixture), "pkg/impl.py", &[]);
    let text = fixture.text("pkg/impl.py");
    let call_site = text.find("x.run").expect("the call site");

    let at_call: Vec<&SemanticEvidence> = produced
        .evidence
        .iter()
        .filter(|item| {
            item.occurrence
                .is_some_and(|site| site.start_byte == call_site)
        })
        .collect();
    assert_eq!(at_call.len(), 1, "one site, one relation");
    assert_eq!(at_call[0].relation_kind, Some(RelationKind::Calls));
    assert!(
        produced
            .evidence
            .iter()
            .filter(|item| item.relation_kind == Some(RelationKind::References))
            .all(|item| item
                .occurrence
                .is_some_and(|site| site.kind == OccurrenceKind::ReferenceSite)),
        "REFERENCES only ever comes from a reference site"
    );
}

#[test]
fn a_derived_override_does_not_outlive_the_ancestor_that_proved_it() {
    let fixture = Fixture::create("dependency");
    let published = refresh(&fixture, &impl_backend(&fixture), "pkg/impl.py").expect("published");
    let impl_run = symbol(&fixture, "pkg/impl.py", "Impl.run");
    assert_eq!(overrides_of(&fixture, impl_run).len(), 1);
    // The proof names the ancestor's Resource, which is what makes a
    // change over there able to invalidate the edge over here.
    assert!(
        published
            .publication
            .basis
            .sources
            .contains_key(&fixture.resource("pkg/base.py").id)
    );

    // Re-extracting a Resource whose declarations another context's
    // semantic relations point at needs that contribution withdrawn
    // first: `semantic_evidence.relation_id` has no ON DELETE CASCADE,
    // unlike `occurrence_id`. Task 4 provides `withdraw` for exactly
    // this, and it is the ordering task 8's lifecycle has to keep.
    let store = GraphStore::open(&fixture.db_path()).expect("index.db");
    crate::merge::withdraw(store.connection(), &context().context_key(), None).expect("withdraw");
    drop(store);

    // The base member is renamed, and the structural index catches up.
    fs::write(
        fixture.root.join("pkg/base.py"),
        "class Base:\n    def renamed(self, value: int) -> str:\n        ...\n",
    )
    .expect("rewrite the base");
    crate::scan::BaselineScan::open(&fixture.db_path())
        .expect("index.db")
        .run_initial_scan(
            &fixture.root,
            &crate::config::WorkspaceConfig::default(),
            "workspace-rev-2",
        )
        .expect("reindex");
    assert!(
        crate::symbol::SymbolStore::open(&fixture.db_path())
            .expect("index.db")
            .list_for_resource(fixture.resource("pkg/base.py").id)
            .expect("symbols")
            .iter()
            .all(|symbol| symbol.name != "run"),
        "the ancestor member really is gone"
    );

    // Re-deriving against the Workspace as it is now finds no ancestor
    // member, so the edge is not recreated. A derived relation is only
    // ever as alive as the facts under it.
    // A backend built against the file as it is now: `Base.run` is
    // simply not there to answer about any more.
    let after = ScriptedBackend::new().with_search_paths(Vec::new());
    refresh(&fixture, &after, "pkg/impl.py").expect("republished");
    assert!(
        overrides_of(&fixture, symbol(&fixture, "pkg/impl.py", "Impl.run")).is_empty(),
        "the derivation re-evaluates rather than remembering"
    );
}

#[test]
fn an_ancestor_change_makes_the_publication_not_current() {
    let fixture = Fixture::create("invalidate");
    refresh(&fixture, &impl_backend(&fixture), "pkg/impl.py").expect("published");
    let index = SemanticIndex::open(&fixture.db_path()).expect("index.db");
    let key = context().context_key();
    assert_eq!(
        index.status(&key).expect("status").state,
        SemanticState::Current
    );

    // The ancestor's Resource is in the basis, so invalidating it
    // reaches the context that depended on it -- and an override whose
    // proof moved stops reading as current without anything having to
    // recompute it first.
    let touched = index
        .invalidate_resource(fixture.resource("pkg/base.py").id)
        .expect("invalidate");
    assert!(touched.contains(&key), "invalidated {touched:?}");
    assert_eq!(
        index.status(&key).expect("status").state,
        SemanticState::Dirty
    );
}

#[test]
fn derived_overrides_are_idempotent_across_identical_refreshes() {
    let fixture = Fixture::create("ov-idempotent");
    refresh(&fixture, &impl_backend(&fixture), "pkg/shapes.py").expect("published");
    let relations = count(&fixture, "relation");
    let evidence = count(&fixture, "semantic_evidence");

    let again = refresh(&fixture, &impl_backend(&fixture), "pkg/shapes.py").expect("republished");
    assert_eq!(count(&fixture, "relation"), relations);
    assert_eq!(count(&fixture, "semantic_evidence"), evidence);
    assert_eq!(again.merged.relations_created, 0);
    assert_eq!(again.merged.relations_removed, 0);
    assert_eq!(
        overrides_of(
            &fixture,
            symbol(&fixture, "pkg/shapes.py", "Concrete.compute")
        )
        .len(),
        1
    );
}

#[test]
fn the_enriched_graph_answers_through_the_existing_surfaces() {
    let fixture = Fixture::create("surfaces");
    refresh(&fixture, &impl_backend(&fixture), "pkg/impl.py").expect("published");
    let base_run = GraphEndpoint::Symbol(symbol(&fixture, "pkg/base.py", "Base.run"));

    // Impact: a public signature change on the base member reaches the
    // overriding declaration, with no Python-specific traversal.
    let impact = crate::impact::ImpactTraversal::open(&fixture.db_path())
        .expect("index.db")
        .run(
            crate::impact::ImpactIntent::PublicSignatureChange,
            &base_run,
            &crate::impact::Budget::default(),
        )
        .expect("impact");
    let impl_run = GraphEndpoint::Symbol(symbol(&fixture, "pkg/impl.py", "Impl.run"));
    assert!(
        impact.nodes.iter().any(|node| node.endpoint == impl_run),
        "the derived override participates in typed impact"
    );

    // Callers: the receiver-typed call resolved in task 6 answers here.
    assert!(
        RelationIndex::open(&fixture.db_path())
            .expect("index.db")
            .callers(&base_run)
            .expect("callers")
            .confirmed_count()
            > 0
    );

    // Prepared inspection returns the current declaration source, not
    // a line number for the Agent to go and read.
    let prepared = crate::prepare::InspectPreparer::open(&fixture.db_path(), &fixture.root)
        .expect("preparer")
        .prepare(
            &base_run,
            crate::relations::Direction::Incoming,
            &[RelationKind::Overrides],
        )
        .expect("prepared");
    assert!(prepared.confirmed_count() > 0);
    assert!(
        prepared
            .ranges
            .iter()
            .any(|range| range.source.contains("def run")),
        "the overriding declaration comes back as source"
    );
}

#[test]
fn a_reference_site_structural_resolution_could_not_follow_becomes_references() {
    let fixture = Fixture::create("references");
    let uri = fixture.uri("pkg/uses.py");
    // `from .reexport import Exported`, where `reexport.py` only
    // aliases it -- a chain I3 stops at and reports as a gap.
    let backend = ScriptedBackend::new()
        .with_search_paths(Vec::new())
        .with_definition(
            &uri,
            last_character(&fixture, "pkg/uses.py", "Exported", 1),
            vec![fixture.location("pkg/base.py", "Base", 0)],
        );
    let outcome = refresh(&fixture, &backend, "pkg/uses.py").expect("published");
    assert!(outcome.merged.gaps_resolved > 0);

    let base = symbol(&fixture, "pkg/base.py", "Base");
    let referrers = RelationIndex::open(&fixture.db_path())
        .expect("index.db")
        .references(&GraphEndpoint::Symbol(base))
        .expect("references");
    assert_eq!(referrers.confirmed_count(), 1);
    assert_eq!(referrers.confirmed[0].kind, RelationKind::References);
    assert_eq!(
        referrers.confirmed[0].evidence[0].occurrence_kind,
        OccurrenceKind::ReferenceSite,
        "a reference site, never a call or an import site"
    );

    // The exact current span, so a reader gets source rather than a
    // line number.
    let text = fixture.text("pkg/uses.py");
    let span = referrers.confirmed[0].evidence[0].span;
    assert_eq!(&text[span.start_byte..span.end_byte], "Exported");
}
