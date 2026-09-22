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
    let asked = backend
        .calls()
        .into_iter()
        .find_map(|request| match request {
            PythonRequest::Definition { position, .. } => Some(position),
            _ => None,
        })
        .expect("a definition was asked");
    let line_start = text[..call_site].rfind('\n').map_or(0, |index| index + 1);
    assert_eq!(
        usize::try_from(asked.character).expect("fits"),
        text[line_start..call_site + "x.run".len() - 1]
            .encode_utf16()
            .count()
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

    // Task 5 measured both of these answering MethodNotFound, so they
    // have to be derived -- that is task 7, not a claim made here.
    for unsupported in [
        SemanticCapability::Implements,
        SemanticCapability::Overrides,
        SemanticCapability::ImplementationTarget,
        SemanticCapability::Inheritance,
    ] {
        assert_eq!(
            report.support(unsupported),
            Support::Unsupported,
            "{unsupported:?}"
        );
    }
}

#[test]
fn inheritance_and_override_gaps_are_left_for_task_seven() {
    let fixture = Fixture::create("deferred");
    let text = fixture.text("pkg/impl.py");
    let base_site = text.rfind("Base").expect("a Base type site");
    let overrides = PersistedUnresolved {
        occurrence: OccurrenceRef {
            kind: OccurrenceKind::TypeSite,
            start_byte: base_site,
            end_byte: base_site + 4,
        },
        intended: IntendedRelation::Known(RelationKind::Overrides),
        lookup_name: "run".to_owned(),
        module_hint: None,
        reason: UnresolvedReason::OverrideTargetRequiresSemantics,
        candidate_truncated: false,
        candidates: Vec::new(),
        resolution_context_key: None,
    };
    let inheritance = PersistedUnresolved {
        intended: IntendedRelation::Inheritance,
        ..overrides.clone()
    };

    let produced = resolve(
        &fixture,
        &impl_backend(&fixture),
        "pkg/impl.py",
        &[overrides, inheritance],
    );
    assert_eq!(produced.deferred.len(), 2);
    assert!(
        produced
            .evidence
            .iter()
            .all(|item| item.relation_kind != Some(RelationKind::Overrides)),
        "no OVERRIDES is claimed from a backend that cannot answer it"
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
