//! Re-measure the I0/I2/I3 representative scenario under the Python
//! semantic tier, and measure what the semantic tier itself costs
//! (#19 task 9).
//!
//! Same scenario file, same fixture snapshot, same JSONL schema
//! ([`brainprint_engine::telemetry`]) as
//! `benchmarks/baselines/python-signature-impact.brainprint-i3.jsonl`,
//! so the lines can be put side by side without re-deriving anything.
//!
//! ## Why one number would be a lie
//!
//! I3's comparable line is a warm query over a persisted index. I4 adds
//! a *process*: a Node child, a handshake, a protocol negotiation, a
//! project analysis, and a per-owner refresh. Folding all of that into
//! the warm query line would hide exactly the cost a reader is trying
//! to see, so the harness emits separate variants:
//!
//! | variant | what it measures |
//! |---|---|
//! | `brainprint-i3-control` | the I3 query flow re-run at this commit |
//! | `brainprint-i4-python-start` | spawn + handshake + negotiation + readiness |
//! | `brainprint-i4-python-first-refresh` | the first owner's semantic refresh |
//! | `brainprint-i4-python` | the same Agent-facing query, semantics current |
//! | `brainprint-i4-python-save` | write → structural current → semantic current |
//! | `brainprint-i4-python-config` | config change → semantic current |
//! | `brainprint-i4-python-restart` | crash → restart → refresh only what needs it |
//!
//! ## The claim this harness is built to falsify
//!
//! The architecture says a warm Agent question reads persisted
//! canonical truth and does **not** wake the type server. That is not
//! asserted here from code shape: the supervisor's
//! `requests_started` counter is sampled either side of the warm query
//! and reported as `backend_requests=`. A non-zero value is printed,
//! not hidden.
//!
//! ## Timing
//!
//! Warm-query timing is repeated (`--repeat`, default 9) and reported
//! as individual observations plus median and p95; the recorded
//! `elapsed_ms` is the median. Cold start and refresh are expensive and
//! run once, with `samples=1` in the notes. No claim is made from any
//! of it beyond "this is what this machine did on this day".
//!
//! `process_cpu_ms`/`peak_rss_bytes` stay `null` for the same reason
//! they are null on the I2/I3 lines: the Workspace denies
//! `unsafe_code`. Wrap the binary in `/usr/bin/time -l` (macOS) or
//! `-v` (GNU) for the process-level figures.
//!
//! Without the pinned install the semantic variants are skipped and the
//! structural control still runs -- which is itself the Level B claim.
//!
//! Run:
//! ```text
//! cargo run -p brainprint-engine --example i4_python_benchmark -- \
//!   --scenario benchmarks/scenarios/python-signature-impact.json \
//!   --output benchmarks/reports/i4-python.jsonl
//! ```

use std::{
    collections::BTreeSet,
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
    impact::{Budget, ImpactIntent, ImpactTraversal},
    prepare::{InspectPreparer, PreparedInspection},
    python_semantic::{
        BatchPolicy, LeaseQueries, PyrightInstall, PythonLauncher, PythonSettings, RefreshRequest,
        capability_report, lifecycle, refresh_resource, toolchain_identity,
    },
    query::{QueryIndex, SymbolQuery, SymbolSelector},
    reconcile::Reconcile,
    related_tests::{ProjectionOutcome, RelatedTests},
    relations::Direction,
    resource::{ResourceLanguage, ResourceStore},
    runtime::{RequestOptions, RuntimePolicy, SemanticRuntimeSupervisor},
    scan::BaselineScan,
    semantic::{AnalysisContext, AnalysisContextBinding, ProjectRootIdentity, SemanticBackendKind},
    semantic_index::{SemanticIndex, SemanticOwner, SemanticState},
    telemetry::{BenchmarkMetrics, BenchmarkRecorder, BenchmarkResult},
};
use rusqlite::Connection;
use serde_json::Value;

type Failure = Box<dyn std::error::Error>;

fn main() -> Result<(), Failure> {
    let mut scenario_path = None;
    let mut output_path = None;
    let mut repeat = 9_usize;
    let mut args = env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--scenario" => scenario_path = args.next(),
            "--output" => output_path = args.next(),
            "--repeat" => repeat = args.next().ok_or("--repeat needs a value")?.parse()?,
            other => return Err(format!("unexpected argument {other:?}").into()),
        }
    }
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let scenario_path = repo_root.join(scenario_path.ok_or("--scenario is required")?);
    let output_path = repo_root.join(output_path.ok_or("--output is required")?);

    let scenario: Value = serde_json::from_slice(&fs::read(&scenario_path)?)?;
    let scenario_id = scenario["id"].as_str().ok_or("scenario id")?.to_owned();
    let query = scenario["query"]
        .as_str()
        .ok_or("scenario query")?
        .to_owned();
    let fixture = repo_root.join(scenario["workspace"].as_str().ok_or("workspace")?);
    let expected: BTreeSet<String> = scenario["expected_matches"]
        .as_array()
        .ok_or("expected matches")?
        .iter()
        .filter_map(|value| value.as_str().map(ToOwned::to_owned))
        .collect();

    let scratch = env::temp_dir().join(format!("brainprint-i4-benchmark-{}", process::id()));
    let _ = fs::remove_dir_all(&scratch);
    let workspace_root = scratch.join("workspace");
    copy_tree(&fixture, &workspace_root)?;
    let db_path = scratch.join("data").join("index.db");

    let mut results = Vec::new();

    // ---- The structural control, re-run at this commit. ------------
    let index_started = Instant::now();
    BaselineScan::open(&db_path)?.run_initial_scan(
        &workspace_root,
        &WorkspaceConfig::default(),
        "benchmark-rev-1",
    )?;
    let index_elapsed = elapsed_ms(index_started);
    let (index_bytes, index_lines) = read_totals(&every_file(&workspace_root)?)?;

    let structural_before = accuracy(&db_path)?;
    let mut structural_runs = Vec::new();
    let mut structural = None;
    for _ in 0..repeat {
        let observed = agent_query(&db_path, &workspace_root, &query, &expected)?;
        structural_runs.push(observed.elapsed_ms);
        structural = Some(observed);
    }
    let structural = structural.ok_or("--repeat must be at least 1")?;
    structural_runs.sort_unstable();

    results.push(result(
        &scenario_id,
        "brainprint-i4-index",
        index_elapsed,
        BenchmarkMetrics {
            tool_calls: Some(1),
            source_read_bytes: Some(index_bytes),
            source_read_lines: Some(index_lines),
            duplicate_read_bytes: Some(0),
            process_cpu_ms: None,
            peak_rss_bytes: None,
        },
        true,
        &format!(
            "baseline scan over the whole Workspace; files_scanned={}; \
             structural cost, unchanged by I4",
            every_file(&workspace_root)?.len()
        ),
    ));
    results.push(result(
        &scenario_id,
        "brainprint-i3-control",
        median(&structural_runs),
        structural.metrics(),
        structural.success,
        &format!(
            "{}; {}; backend_requests=0; semantic_state=none; \
             samples={}; observations_ms={}; p50_ms={}; p95_ms={}",
            structural.notes(),
            structural_before.notes("structural"),
            structural_runs.len(),
            join_numbers(&structural_runs),
            median(&structural_runs),
            percentile(&structural_runs, 95)
        ),
    ));

    // ---- The semantic tier, if the pinned backend is installed. ----
    let install_root = repo_root.join("scripts").join("python_semantic_spike");
    match PyrightInstall::locate(&install_root, "node") {
        Err(error) => {
            println!(
                "semantic variants skipped: no pinned install under {} ({error})",
                install_root.display()
            );
            println!("this is the Level B path: the structural line above still ran");
        }
        Ok(install) => {
            let semantic = measure_semantic(
                &SemanticInputs {
                    scenario_id: &scenario_id,
                    query: &query,
                    expected: &expected,
                    workspace_root: &workspace_root,
                    db_path: &db_path,
                    install,
                    repeat,
                },
                &structural_before,
            )?;
            results.extend(semantic);
        }
    }

    write_results(&output_path, &results)?;
    let _ = fs::remove_dir_all(&scratch);

    for result in &results {
        println!(
            "{} success={} tool_calls={:?} source_read_bytes={:?} elapsed_ms={}",
            result.variant,
            result.success,
            result.metrics.tool_calls,
            result.metrics.source_read_bytes,
            result.elapsed_ms
        );
        println!("  {}", result.notes.as_deref().unwrap_or(""));
    }
    if results.iter().all(|result| result.success) {
        Ok(())
    } else {
        Err("acceptance mismatch".into())
    }
}

// ---------------------------------------------------------------------
// The Agent-facing query flow -- identical for both tiers
// ---------------------------------------------------------------------

/// One run of the exact workflow the I0/I2/I3 lines measure: *what is
/// this, who calls it, which test covers it, and what source do I need
/// to edit?*
struct AgentQuery {
    elapsed_ms: u64,
    tool_calls: u64,
    read_bytes: u64,
    read_lines: u64,
    files_read: usize,
    prepared_bytes: u64,
    prepared_ranges: usize,
    confirmed: usize,
    gaps: usize,
    traversal_nodes: usize,
    traversal_edges: usize,
    definition_path: String,
    callers: BTreeSet<String>,
    tests: BTreeSet<String>,
    missing: Vec<String>,
    success: bool,
}

impl AgentQuery {
    fn metrics(&self) -> BenchmarkMetrics {
        BenchmarkMetrics {
            tool_calls: Some(self.tool_calls),
            source_read_bytes: Some(self.read_bytes),
            source_read_lines: Some(self.read_lines),
            duplicate_read_bytes: Some(0),
            process_cpu_ms: None,
            peak_rss_bytes: None,
        }
    }

    fn notes(&self) -> String {
        format!(
            "definition={}; callers={}; related_test={}; relation_queries=1; \
             prepared_ranges={}; prepared_source_bytes={}; confirmed_relations={}; \
             gaps={}; traversal_nodes={}; traversal_edges={}; files_read={}; \
             broad_searches=0; repeated_reads=0; missing={}",
            self.definition_path,
            join(&self.callers),
            join(&self.tests),
            self.prepared_ranges,
            self.prepared_bytes,
            self.confirmed,
            self.gaps,
            self.traversal_nodes,
            self.traversal_edges,
            self.files_read,
            if self.missing.is_empty() {
                "none".to_owned()
            } else {
                self.missing.join(",")
            }
        )
    }
}

fn agent_query(
    db_path: &Path,
    workspace_root: &Path,
    query: &str,
    expected: &BTreeSet<String>,
) -> Result<AgentQuery, Failure> {
    let started = Instant::now();

    // Call 1: locate the Symbol by name.
    let index = QueryIndex::open(db_path)?;
    let located = index.search_symbols(&SymbolQuery::new(SymbolSelector::Name(query)))?;
    let definition = located
        .candidates
        .iter()
        .find(|candidate| candidate.path_rel.ends_with("profile.py"))
        .ok_or("the definition is not in the structural index")?;
    let definition_path = definition.path_rel.clone();
    let target = GraphEndpoint::Symbol(definition.symbol.id);

    // Call 2: one prepared inspection -- confirmed callers, exact
    // evidence spans, the declarations they sit in, current source.
    let preparer = InspectPreparer::open(db_path, workspace_root)?;
    let prepared = preparer.prepare(&target, Direction::Incoming, &[RelationKind::Calls])?;

    // Call 3: related tests, projected from confirmed relation paths.
    let budget = Budget::default();
    let related = RelatedTests::open(db_path)?.for_target(
        &target,
        ImpactIntent::PublicSignatureChange,
        &budget,
    )?;
    let elapsed_ms = elapsed_ms(started);

    let impact = ImpactTraversal::open(db_path)?.run(
        ImpactIntent::PublicSignatureChange,
        &target,
        &budget,
    )?;

    let read_files: BTreeSet<String> = prepared
        .ranges
        .iter()
        .map(|range| range.path_rel.clone())
        .collect();
    let (read_bytes, read_lines) = read_totals(
        &read_files
            .iter()
            .map(|path| workspace_root.join(path))
            .collect::<Vec<_>>(),
    )?;
    let prepared_bytes: u64 = prepared
        .ranges
        .iter()
        .map(|range| range.source.len() as u64)
        .sum();

    let resources = ResourceStore::open(db_path)?;
    let mut accounted: BTreeSet<String> = BTreeSet::new();
    accounted.insert(definition_path.clone());
    let mut callers: BTreeSet<String> = BTreeSet::new();
    for relation in &prepared.relations {
        for evidence in &relation.evidence {
            let path = resources
                .get_by_id(evidence.location.resource)?
                .ok_or("evidence owner")?
                .path_rel;
            callers.insert(path.clone());
            accounted.insert(path);
        }
    }
    let tests: BTreeSet<String> = related
        .candidates
        .iter()
        .map(|candidate| candidate.path_rel.clone())
        .collect();
    let missing: Vec<String> = expected.difference(&accounted).cloned().collect();
    let definition_source = prepared
        .target
        .as_ref()
        .and_then(|target| target.range)
        .and_then(|id| prepared.range(id))
        .map(|range| range.source.clone())
        .unwrap_or_default();

    let success = missing.is_empty()
        && callers.len() == 3
        && tests.len() == 1
        && related.outcome() == ProjectionOutcome::Candidates
        && definition_source.starts_with(&format!("def {query}("))
        && prepared.source_complete()
        && all_confirmed(&prepared);

    Ok(AgentQuery {
        elapsed_ms,
        tool_calls: 3,
        read_bytes,
        read_lines,
        files_read: read_files.len(),
        prepared_bytes,
        prepared_ranges: prepared.ranges.len(),
        confirmed: prepared.confirmed_count(),
        gaps: prepared.gaps.len(),
        traversal_nodes: impact.nodes.len(),
        traversal_edges: impact.edges.len(),
        definition_path,
        callers,
        tests,
        missing,
        success,
    })
}

/// Whether every prepared relation is a confirmed, current edge whose
/// evidence has a real source range behind it.
fn all_confirmed(prepared: &PreparedInspection) -> bool {
    prepared.relations.iter().all(|relation| {
        relation.relation.resolution == brainprint_engine::resolution::Resolution::Resolved
            && relation
                .evidence
                .iter()
                .all(|evidence| evidence.evidence_range.is_some())
    })
}

// ---------------------------------------------------------------------
// Accuracy -- what each tier actually proved
// ---------------------------------------------------------------------

/// The graph's proof census, so "I4 resolved what I3 could not" is a
/// counted claim rather than a narrative one.
struct Accuracy {
    confirmed: i64,
    candidates: i64,
    unresolved: i64,
    requires_semantics: i64,
    /// Confirmed relations per kind, and unresolved per reason. Without
    /// these, "7 gaps became 2" says nothing about *which* question the
    /// semantic tier answered.
    by_kind: Vec<(String, i64)>,
    by_reason: Vec<(String, i64)>,
}

impl Accuracy {
    fn notes(&self, label: &str) -> String {
        format!(
            "{label}_confirmed={}; {label}_candidates={}; {label}_unresolved={}; \
             {label}_requires_semantics={}; {label}_kinds={}; {label}_reasons={}",
            self.confirmed,
            self.candidates,
            self.unresolved,
            self.requires_semantics,
            pairs(&self.by_kind),
            pairs(&self.by_reason)
        )
    }
}

/// The reasons [`brainprint_engine::gaps`] classifies as needing a
/// semantic backend. Spelled out here rather than imported so the
/// benchmark counts exactly what it says it counts.
const REQUIRES_SEMANTICS: &str = "'MODULE_TREE_REQUIRES_SEMANTICS','NAMESPACE_REQUIRES_SEMANTICS',\
     'RECEIVER_TYPE_REQUIRED','TYPE_SEMANTICS_REQUIRED',\
     'OVERRIDE_TARGET_REQUIRES_SEMANTICS','CONFIG_DEPENDENT_SPECIFIER'";

fn accuracy(db_path: &Path) -> Result<Accuracy, Failure> {
    let connection = Connection::open(db_path)?;
    let count = |sql: String| -> Result<i64, Failure> {
        Ok(connection.query_row(&sql, [], |row| row.get(0))?)
    };
    let grouped = |sql: &str| -> Result<Vec<(String, i64)>, Failure> {
        let mut statement = connection.prepare(sql)?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    };
    Ok(Accuracy {
        confirmed: count("SELECT COUNT(*) FROM relation".to_owned())?,
        candidates: count("SELECT COUNT(*) FROM relation_candidate".to_owned())?,
        unresolved: count("SELECT COUNT(*) FROM unresolved_reference".to_owned())?,
        requires_semantics: count(format!(
            "SELECT COUNT(*) FROM unresolved_reference WHERE reason IN ({REQUIRES_SEMANTICS})"
        ))?,
        by_kind: grouped("SELECT kind, COUNT(*) FROM relation GROUP BY kind ORDER BY kind")?,
        by_reason: grouped(
            "SELECT reason, COUNT(*) FROM unresolved_reference GROUP BY reason ORDER BY reason",
        )?,
    })
}

fn pairs(values: &[(String, i64)]) -> String {
    values
        .iter()
        .map(|(name, count)| format!("{name}:{count}"))
        .collect::<Vec<_>>()
        .join("+")
}

// ---------------------------------------------------------------------
// The semantic tier
// ---------------------------------------------------------------------

struct SemanticInputs<'a> {
    scenario_id: &'a str,
    query: &'a str,
    expected: &'a BTreeSet<String>,
    workspace_root: &'a Path,
    db_path: &'a Path,
    install: PyrightInstall,
    repeat: usize,
}

#[allow(clippy::too_many_lines)]
fn measure_semantic(
    inputs: &SemanticInputs<'_>,
    structural_before: &Accuracy,
) -> Result<Vec<BenchmarkResult>, Failure> {
    let settings = PythonSettings::default();
    let environment = lifecycle::environment_identity(inputs.workspace_root, &settings);
    let package_version = inputs.install.package_version.clone();
    let context = AnalysisContext {
        workspace: WorkspaceId::from_bytes([19; 16]),
        backend: SemanticBackendKind::Python,
        language: ResourceLanguage::Python,
        project_root: ProjectRootIdentity::Key("python-signature-impact".to_owned()),
        toolchain: toolchain_identity(&inputs.install, &environment),
    };
    let binding = AnalysisContextBinding {
        context: context.clone(),
        project_root_rel: String::new(),
        config_file_rel: None,
    };
    // A zero idle window so the outage below is deterministic: nothing
    // unloads on its own, but one explicit sweep stops the backend
    // exactly the way the idle policy would.
    let supervisor = SemanticRuntimeSupervisor::new(RuntimePolicy {
        idle_timeout: Duration::ZERO,
        ..RuntimePolicy::default()
    })
    .with_backend(Arc::new(PythonLauncher::new(
        inputs.install.clone(),
        inputs.workspace_root.to_path_buf(),
        settings.clone(),
    )));

    // --- 2. Cold start: spawn, handshake, negotiate, become ready.
    let cold = Instant::now();
    let lease = supervisor.acquire(&binding)?;
    let start_elapsed = elapsed_ms(cold);
    let protocol_version = lease
        .context()
        .toolchain
        .backend_compatibility_class
        .clone();
    let live = supervisor.live_runtime_count();
    let after_start = supervisor
        .telemetry(&context.context_key())
        .ok_or("telemetry")?;

    let queries = LeaseQueries::new(
        &lease,
        RequestOptions::with_timeout(Duration::from_secs(60)),
    );
    let index = SemanticIndex::open(inputs.db_path)?;
    let discovered = lifecycle::discover_config(index.connection(), inputs.workspace_root, "")?;
    let config = discovered.basis(&settings);
    let capabilities = capability_report(&context);
    let resources = ResourceStore::open(inputs.db_path)?;
    let python_owners: Vec<_> = resources
        .list_active()?
        .into_iter()
        .filter(|resource| resource.language == Some(ResourceLanguage::Python))
        .collect();

    let refresh_one = |rel: &str| -> Result<_, Failure> {
        let owner = python_owners
            .iter()
            .find(|resource| resource.path_key == rel)
            .ok_or_else(|| format!("{rel} is not indexed"))?;
        Ok(refresh_resource(
            &index,
            &queries,
            &RefreshRequest {
                context: &context,
                workspace_root: inputs.workspace_root,
                owner: owner.id,
                config: &config,
                capabilities: &capabilities,
                policy: BatchPolicy { max_attempts: 12 },
            },
        )?)
    };

    // --- 3. The first owner refresh, on its own.
    let first_started = Instant::now();
    let first = refresh_one("src/profile_app/profile.py")?;
    let first_elapsed = elapsed_ms(first_started);
    let after_first = supervisor
        .telemetry(&context.context_key())
        .ok_or("telemetry")?;

    // The rest of the package, so the warm query runs against a fully
    // current context rather than one lucky file.
    let mut refresh_total = first.evidence_count;
    for resource in &python_owners {
        if resource.path_key == "src/profile_app/profile.py" {
            continue;
        }
        refresh_total += refresh_one(&resource.path_key)?.evidence_count;
    }
    let after_all = supervisor
        .telemetry(&context.context_key())
        .ok_or("telemetry")?;
    let semantic_after = accuracy(inputs.db_path)?;

    // --- 4. The warm Agent query, semantics current.
    //     The question this answers: does asking cost a backend call?
    let before_warm = supervisor
        .telemetry(&context.context_key())
        .ok_or("telemetry")?
        .requests_started;
    let mut warm_runs = Vec::new();
    let mut warm = None;
    for _ in 0..inputs.repeat {
        let observed = agent_query(
            inputs.db_path,
            inputs.workspace_root,
            inputs.query,
            inputs.expected,
        )?;
        warm_runs.push(observed.elapsed_ms);
        warm = Some(observed);
    }
    let warm = warm.ok_or("--repeat must be at least 1")?;
    warm_runs.sort_unstable();
    let warm_backend_requests = supervisor
        .telemetry(&context.context_key())
        .ok_or("telemetry")?
        .requests_started
        - before_warm;

    // --- 5. One file is saved. How long until semantics are current?
    let save = measure_save(&MutationInputs {
        index: &index,
        queries: &queries,
        context: &context,
        config: &config,
        capabilities: &capabilities,
        workspace_root: inputs.workspace_root,
        db_path: inputs.db_path,
        supervisor: &supervisor,
    })?;

    // --- 6. The selected configuration changes.
    let config_change = measure_config_change(&MutationInputs {
        index: &index,
        queries: &queries,
        context: &context,
        config: &config,
        capabilities: &capabilities,
        workspace_root: inputs.workspace_root,
        db_path: inputs.db_path,
        supervisor: &supervisor,
    })?;

    // --- 7. The backend goes away and comes back.
    // The lease's borrow ends here; the sweep is what actually stops
    // the process.
    drop(lease);
    let unloaded = supervisor.sweep_idle();
    let structural_during_outage = agent_query(
        inputs.db_path,
        inputs.workspace_root,
        inputs.query,
        inputs.expected,
    )?;
    let restart_started = Instant::now();
    let lease = supervisor.acquire(&binding)?;
    let restart_elapsed = elapsed_ms(restart_started);
    let after_restart = supervisor
        .telemetry(&context.context_key())
        .ok_or("telemetry")?;
    let queries = LeaseQueries::new(
        &lease,
        RequestOptions::with_timeout(Duration::from_secs(60)),
    );
    let owner = python_owners
        .iter()
        .find(|resource| resource.path_key == "src/profile_app/profile.py")
        .ok_or("profile.py")?;
    let refresh_after_restart = Instant::now();
    refresh_resource(
        &index,
        &queries,
        &RefreshRequest {
            context: &context,
            workspace_root: inputs.workspace_root,
            owner: owner.id,
            config: &config,
            capabilities: &capabilities,
            policy: BatchPolicy { max_attempts: 12 },
        },
    )?;
    let refresh_after_restart = elapsed_ms(refresh_after_restart);
    drop(lease);
    supervisor.shutdown();

    Ok(vec![
        result(
            inputs.scenario_id,
            "brainprint-i4-python-start",
            start_elapsed,
            BenchmarkMetrics {
                tool_calls: Some(1),
                source_read_bytes: Some(0),
                source_read_lines: Some(0),
                duplicate_read_bytes: Some(0),
                process_cpu_ms: None,
                peak_rss_bytes: None,
            },
            live == 1,
            &format!(
                "spawn+handshake+negotiate+ready; backend={}; compatibility_class={protocol_version}; \
                 live_runtimes={live}; starts_attempted={}; starts_succeeded={}; \
                 backend_requests={}; samples=1; \
                 this is the cost I4 adds before any semantic answer",
                package_version,
                after_start.starts_attempted,
                after_start.starts_succeeded,
                after_start.requests_started
            ),
        ),
        result(
            inputs.scenario_id,
            "brainprint-i4-python-first-refresh",
            first_elapsed,
            BenchmarkMetrics {
                tool_calls: Some(1),
                source_read_bytes: Some(0),
                source_read_lines: Some(0),
                duplicate_read_bytes: Some(0),
                process_cpu_ms: None,
                peak_rss_bytes: None,
            },
            first.evidence_count > 0,
            &format!(
                "owner=src/profile_app/profile.py; evidence={}; gaps_resolved={}; \
                 relations_created={}; deferred={}; backend_requests={}; \
                 all_owners_refreshed={}; all_owners_evidence={refresh_total}; samples=1",
                first.evidence_count,
                first.merged.gaps_resolved,
                first.merged.relations_created,
                first.deferred.len(),
                after_first.requests_started - after_start.requests_started,
                python_owners.len(),
            ),
        ),
        result(
            inputs.scenario_id,
            "brainprint-i4-python",
            median(&warm_runs),
            warm.metrics(),
            warm.success && warm_backend_requests == 0,
            &format!(
                "{}; {}; {}; backend_requests={warm_backend_requests}; \
                 refresh_backend_requests={}; samples={}; observations_ms={}; p50_ms={}; p95_ms={}",
                warm.notes(),
                structural_before.notes("i3"),
                semantic_after.notes("i4"),
                after_all.requests_started,
                warm_runs.len(),
                join_numbers(&warm_runs),
                median(&warm_runs),
                percentile(&warm_runs, 95)
            ),
        ),
        result(
            inputs.scenario_id,
            "brainprint-i4-python-save",
            save.elapsed_ms,
            BenchmarkMetrics {
                tool_calls: Some(1),
                source_read_bytes: Some(0),
                source_read_lines: Some(0),
                duplicate_read_bytes: Some(0),
                process_cpu_ms: None,
                peak_rss_bytes: None,
            },
            save.stale_current_incidents == 0 && save.unrelated_refreshed == 0,
            &save.notes(),
        ),
        result(
            inputs.scenario_id,
            "brainprint-i4-python-config",
            config_change.elapsed_ms,
            BenchmarkMetrics {
                tool_calls: Some(1),
                source_read_bytes: Some(0),
                source_read_lines: Some(0),
                duplicate_read_bytes: Some(0),
                process_cpu_ms: None,
                peak_rss_bytes: None,
            },
            config_change.stale_current_incidents == 0,
            &config_change.notes(),
        ),
        result(
            inputs.scenario_id,
            "brainprint-i4-python-restart",
            restart_elapsed + refresh_after_restart,
            BenchmarkMetrics {
                tool_calls: Some(1),
                source_read_bytes: Some(0),
                source_read_lines: Some(0),
                duplicate_read_bytes: Some(0),
                process_cpu_ms: None,
                peak_rss_bytes: None,
            },
            structural_during_outage.success,
            &format!(
                "ready_again_ms={restart_elapsed}; refresh_after_restart_ms={refresh_after_restart}; \
                 owners_refreshed=1; starts_attempted={}; starts_succeeded={}; restart_attempts={}; \
                 crashes={}; backends_unloaded={unloaded}; structural_query_during_outage={}; \
                 structural_confirmed_during_outage={}; samples=1",
                after_restart.starts_attempted,
                after_restart.starts_succeeded,
                after_restart.restart_attempts,
                after_restart.crashes,
                structural_during_outage.success,
                structural_during_outage.confirmed
            ),
        ),
    ])
}

// ---------------------------------------------------------------------
// Lifecycle measurements
// ---------------------------------------------------------------------

struct MutationInputs<'a> {
    index: &'a SemanticIndex,
    queries: &'a LeaseQueries<'a>,
    context: &'a AnalysisContext,
    config: &'a brainprint_engine::semantic_index::ConfigBasis,
    capabilities: &'a brainprint_engine::semantic::CapabilityReport,
    workspace_root: &'a Path,
    db_path: &'a Path,
    supervisor: &'a SemanticRuntimeSupervisor,
}

struct LifecycleCost {
    label: String,
    elapsed_ms: u64,
    affected: usize,
    refreshed: usize,
    unrelated_refreshed: usize,
    notifications: u64,
    restarts: u64,
    backend_requests: u64,
    stale_current_incidents: usize,
}

impl LifecycleCost {
    fn notes(&self) -> String {
        format!(
            "{}; affected_owners={}; refreshed_owners={}; unrelated_owners_refreshed={}; \
             backend_notifications={}; backend_restarts={}; backend_requests={}; \
             stale_current_incidents={}; samples=1",
            self.label,
            self.affected,
            self.refreshed,
            self.unrelated_refreshed,
            self.notifications,
            self.restarts,
            self.backend_requests,
            self.stale_current_incidents
        )
    }
}

/// Write one file, then walk the whole lifecycle to semantic CURRENT.
///
/// The interesting number is not the milliseconds: it is
/// `unrelated_owners_refreshed` and `stale_current_incidents`, which is
/// what the whole owner-level publication model exists to keep at zero.
fn measure_save(inputs: &MutationInputs<'_>) -> Result<LifecycleCost, Failure> {
    let resources = ResourceStore::open(inputs.db_path)?;
    let changed = resources
        .list_active()?
        .into_iter()
        .find(|resource| resource.path_key == "src/profile_app/profile.py")
        .ok_or("profile.py")?;
    let untouched = SemanticOwner::new(
        inputs.context.context_key(),
        resources
            .list_active()?
            .into_iter()
            .find(|resource| resource.path_key == "src/profile_app/config.py")
            .ok_or("config.py")?
            .id,
    );
    let untouched_before = inputs.index.status(&untouched)?.stable_generation_id;

    let source = fs::read_to_string(inputs.workspace_root.join("src/profile_app/profile.py"))?;
    let edited = source.replace("user_id: str", "user_id: str, trace: bool = False");

    let before_requests = telemetry_requests(inputs.supervisor, inputs.context);
    let started = Instant::now();

    let changes = vec![lifecycle::ResourceChange::new(
        changed.id,
        lifecycle::ChangeKind::Changed,
        "src/profile_app/profile.py",
    )];
    let plan = lifecycle::plan_changes(
        inputs.index,
        inputs.context,
        &changes,
        &lifecycle::discover_config(inputs.index.connection(), inputs.workspace_root, "")?,
    )?;
    lifecycle::withdraw_affected(inputs.index, &plan.affected, "SEMANTIC_SOURCE_MOVED")?;
    fs::write(
        inputs.workspace_root.join("src/profile_app/profile.py"),
        &edited,
    )?;

    // The incremental structural path, not a whole-Workspace rescan:
    // a baseline scan replaces the structure of every Resource,
    // including ones whose semantic contribution was never withdrawn,
    // which is not what saving one file does.
    let reconciled =
        Reconcile::open(inputs.db_path)?.run(inputs.workspace_root, &WorkspaceConfig::default())?;
    let structurally_changed = reconciled
        .changes
        .iter()
        .filter(|change| {
            !matches!(
                change,
                brainprint_engine::identity::ResourceChange::Unchanged { .. }
                    | brainprint_engine::identity::ResourceChange::MetadataRefresh { .. }
            )
        })
        .count();
    let watched = lifecycle::watched_changes(inputs.workspace_root, &changes);
    let notifications = watched.len() as u64;
    brainprint_engine::python_semantic::adapter::notify_watched_files(inputs.queries, watched)?;

    // The stale-current gate: between the withdrawal and the refresh,
    // nothing affected may still be claiming CURRENT.
    let mut stale = 0;
    for owner in &plan.affected {
        if inputs.index.status(owner)?.state == SemanticState::Current {
            stale += 1;
        }
    }

    let refreshed = refresh_all(inputs, &plan.affected)?;
    let elapsed_ms = elapsed_ms(started);

    let untouched_after = inputs.index.status(&untouched)?.stable_generation_id;
    Ok(LifecycleCost {
        label: format!(
            "write -> reconcile({structurally_changed} changed) -> notify -> refresh -> publish"
        ),
        elapsed_ms,
        affected: plan.affected.len(),
        refreshed,
        unrelated_refreshed: usize::from(untouched_before != untouched_after),
        notifications,
        restarts: 0,
        backend_requests: telemetry_requests(inputs.supervisor, inputs.context) - before_requests,
        stale_current_incidents: stale,
    })
}

/// Add a `pyrightconfig.json` where none governed before, so *which*
/// file configures the project changes without any source moving.
fn measure_config_change(inputs: &MutationInputs<'_>) -> Result<LifecycleCost, Failure> {
    let before_requests = telemetry_requests(inputs.supervisor, inputs.context);
    let started = Instant::now();

    // Withdrawal comes before structural replacement, here as
    // everywhere: `semantic_evidence.relation_id` has no cascade, and
    // the ordering is the contract that keeps the displaced gaps.
    let affected = lifecycle::invalidate_for_config(inputs.index, inputs.context)?;
    lifecycle::withdraw_affected(inputs.index, &affected, "SEMANTIC_CONFIG_CHANGED")?;
    fs::write(
        inputs.workspace_root.join("pyrightconfig.json"),
        "{\n  \"include\": [\"src\"],\n  \"typeCheckingMode\": \"standard\"\n}\n",
    )?;
    Reconcile::open(inputs.db_path)?.run(inputs.workspace_root, &WorkspaceConfig::default())?;

    let mut stale = 0;
    for owner in &affected {
        if inputs.index.status(owner)?.state == SemanticState::Current {
            stale += 1;
        }
    }

    let discovered =
        lifecycle::discover_config(inputs.index.connection(), inputs.workspace_root, "")?;
    let config = discovered.basis(&PythonSettings::default());
    let refreshed = refresh_all(
        &MutationInputs {
            config: &config,
            ..copy_inputs(inputs)
        },
        &affected,
    )?;
    let elapsed_ms = elapsed_ms(started);

    Ok(LifecycleCost {
        label: "config added -> which file governs changes -> invalidate -> refresh".to_owned(),
        elapsed_ms,
        affected: affected.len(),
        refreshed,
        unrelated_refreshed: 0,
        notifications: 0,
        restarts: 0,
        backend_requests: telemetry_requests(inputs.supervisor, inputs.context) - before_requests,
        stale_current_incidents: stale,
    })
}

const fn copy_inputs<'a>(inputs: &MutationInputs<'a>) -> MutationInputs<'a> {
    MutationInputs {
        index: inputs.index,
        queries: inputs.queries,
        context: inputs.context,
        config: inputs.config,
        capabilities: inputs.capabilities,
        workspace_root: inputs.workspace_root,
        db_path: inputs.db_path,
        supervisor: inputs.supervisor,
    }
}

fn refresh_all(
    inputs: &MutationInputs<'_>,
    owners: &std::collections::BTreeSet<SemanticOwner>,
) -> Result<usize, Failure> {
    let mut request = RefreshRequest {
        context: inputs.context,
        workspace_root: inputs.workspace_root,
        owner: owners
            .iter()
            .next()
            .map(|owner| owner.owner)
            .unwrap_or_else(|| brainprint_core::ResourceId::from_bytes([0; 16])),
        config: inputs.config,
        capabilities: inputs.capabilities,
        policy: BatchPolicy { max_attempts: 12 },
    };
    let outcomes = lifecycle::refresh_owners(inputs.index, inputs.queries, &mut request, owners)?;
    Ok(outcomes
        .iter()
        .filter(|outcome| outcome.succeeded())
        .count())
}

fn telemetry_requests(supervisor: &SemanticRuntimeSupervisor, context: &AnalysisContext) -> u64 {
    supervisor
        .telemetry(&context.context_key())
        .map_or(0, |telemetry| telemetry.requests_started)
}

// ---------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------

fn write_results(output_path: &Path, results: &[BenchmarkResult]) -> Result<(), Failure> {
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut existing = fs::read_to_string(output_path).unwrap_or_default();
    for result in results {
        existing.push_str(&result.to_json_line()?);
    }
    fs::write(output_path, existing)?;
    Ok(())
}

fn result(
    scenario_id: &str,
    variant: &str,
    elapsed_ms: u64,
    metrics: BenchmarkMetrics,
    success: bool,
    notes: &str,
) -> BenchmarkResult {
    let mut recorder = BenchmarkRecorder::new(
        format!("{scenario_id}-{variant}-{}", process::id()),
        scenario_id.to_owned(),
        variant.to_owned(),
    );
    recorder.set_tool_calls(metrics.tool_calls.unwrap_or_default());
    recorder.set_source_reads(
        metrics.source_read_bytes.unwrap_or_default(),
        metrics.source_read_lines.unwrap_or_default(),
        metrics.duplicate_read_bytes.unwrap_or_default(),
    );
    recorder.set_process_usage(metrics.process_cpu_ms, metrics.peak_rss_bytes);
    let mut finished = recorder.finish(success, None, Some(notes.to_owned()));
    finished.elapsed_ms = elapsed_ms;
    finished
}

fn median(sorted: &[u64]) -> u64 {
    percentile(sorted, 50)
}

/// Nearest-rank on an already sorted slice. Small samples only, which
/// is why no interpolation is attempted and the sample count is always
/// printed beside the value.
fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (percentile * sorted.len()).div_ceil(100).max(1);
    sorted[rank.min(sorted.len()) - 1]
}

fn join(paths: &BTreeSet<String>) -> String {
    paths.iter().cloned().collect::<Vec<_>>().join("|")
}

fn join_numbers(values: &[u64]) -> String {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("|")
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn every_file(root: &Path) -> Result<Vec<PathBuf>, Failure> {
    let mut files = Vec::new();
    for entry in
        brainprint_engine::discovery::enumerate_resources(root, &WorkspaceConfig::default())?
    {
        if entry.kind == brainprint_engine::resource::ResourceKind::File {
            files.push(root.join(&entry.path_rel));
        }
    }
    Ok(files)
}

fn read_totals(files: &[PathBuf]) -> Result<(u64, u64), Failure> {
    let mut bytes = 0;
    let mut lines = 0;
    for path in files {
        let data = fs::read(path)?;
        bytes += data.len() as u64;
        lines += line_count(&data);
    }
    Ok((bytes, lines))
}

fn line_count(data: &[u8]) -> u64 {
    if data.is_empty() {
        return 0;
    }
    let newlines = data.iter().filter(|byte| **byte == b'\n').count() as u64;
    if data.ends_with(b"\n") {
        newlines
    } else {
        newlines + 1
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), Failure> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
