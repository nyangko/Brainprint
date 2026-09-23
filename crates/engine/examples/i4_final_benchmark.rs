//! The final I4 measurement: every installed semantic backend, the same
//! shape, and the fleet they make together (#19 task 15).
//!
//! Task 9's `i4_python_benchmark` re-measured the I0/I2/I3 scenario under
//! one backend, in that scenario's schema, and stays exactly as it was --
//! it is the comparable historical line and this harness does not touch
//! it. What was missing was the other four families and the cost of
//! running them together, which is what this measures.
//!
//! ## What each family is asked
//!
//! | phase | what it measures |
//! |---|---|
//! | `-cold` | launcher start → the server says it is ready |
//! | `-first-refresh` | every owner's first semantic refresh |
//! | `-save` | edit → structural current → semantic CURRENT |
//! | `-config` | project/config input change → semantic CURRENT |
//! | `-warm` | the Agent-facing graph query, with no backend running |
//! | `-fleet` | every installed family started in one supervisor |
//!
//! The warm phase runs with every backend **shut down**. That is not a
//! convenience: the architecture's claim is that a warm question reads
//! persisted canonical truth and does not wake a type server, and the
//! cheapest way to prove it is to ask the question when there is no type
//! server to wake. `backend_processes=0` in the notes is that fact.
//!
//! ## What it does not measure
//!
//! `process_cpu_ms` and `peak_rss_bytes` stay `null` on every line, for
//! the reason they are null on the I2/I3/I4-python lines: this Workspace
//! denies `unsafe_code`, so a harness cannot read its own or a child's
//! resource usage. Resource figures come from outside the process:
//! `scripts/i4_final_acceptance/measure_rss.py`, which samples the
//! fixture-owned process tree while `--fleet-hold-ms` holds it up. A
//! metric nobody measured is recorded as unknown and never as zero.
//!
//! Model input/output tokens are not measured at all. No model is
//! involved in this harness, and running a tokenizer over its output
//! would be a number about a tokenizer rather than about an Agent.
//!
//! ## Timing
//!
//! Monotonic (`Instant`). Cold and warm samples are never mixed into one
//! distribution. Every line carries `samples=`, its observations, and
//! `p50`/`p95` where the sample count supports one. No claim is made from
//! any of it beyond "this is what this machine did on this day".
//!
//! Run:
//! ```text
//! cargo run --release -p brainprint-engine --example i4_final_benchmark -- \
//!   --output benchmarks/reports/i4-final.jsonl
//! ```

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use brainprint_core::WorkspaceId;
use brainprint_engine::{
    config::WorkspaceConfig,
    reconcile::Reconcile,
    resource::{Resource, ResourceLanguage, ResourceStore},
    runtime::{
        CancelToken, RequestFailure, RuntimePolicy, RuntimeState, SemanticBackendLauncher,
        SemanticRuntimeHost, SemanticRuntimeSupervisor,
    },
    scan::BaselineScan,
    semantic::{AnalysisContext, AnalysisContextBinding, ProjectRootIdentity, SemanticBackendKind},
    semantic_index::{SemanticIndex, SemanticOwner, SemanticState},
    telemetry::{BenchmarkMetrics, BenchmarkRecorder, BenchmarkResult},
    trust::ProjectExecutionTrust,
};
use rusqlite::Connection;

type Failure = Box<dyn std::error::Error>;

/// The scenario id every line in this run carries. One id, because the
/// families are phases of one final measurement rather than five
/// competing scenarios.
const SCENARIO: &str = "i4-final";

fn main() -> Result<(), Failure> {
    let mut output_path = None;
    let mut warm_repeat = 20_usize;
    let mut cold_samples = 5_usize;
    let mut lifecycle_repeat = 10_usize;
    let mut fleet_hold_ms = 0_u64;
    let mut only: Option<String> = None;
    let mut args = env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--output" => output_path = args.next(),
            "--warm-repeat" => warm_repeat = args.next().ok_or("value")?.parse()?,
            "--cold-samples" => cold_samples = args.next().ok_or("value")?.parse()?,
            "--lifecycle-repeat" => lifecycle_repeat = args.next().ok_or("value")?.parse()?,
            "--fleet-hold-ms" => fleet_hold_ms = args.next().ok_or("value")?.parse()?,
            "--only" => only = args.next(),
            other => return Err(format!("unexpected argument {other:?}").into()),
        }
    }
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let scratch = env::temp_dir().join(format!("brainprint-i4-final-{}", process::id()));
    let _ = fs::remove_dir_all(&scratch);

    let plan = Plan {
        repo_root: repo_root.clone(),
        scratch: scratch.clone(),
        warm_repeat,
        cold_samples,
        lifecycle_repeat,
    };

    // The hold mode exists for the external resource sampler: start
    // every installed family in one supervisor, keep them up while
    // something outside this process reads the tree, then stop.
    if fleet_hold_ms > 0 {
        hold_the_fleet(&plan, fleet_hold_ms)?;
        let _ = fs::remove_dir_all(&scratch);
        return Ok(());
    }

    let mut results = Vec::new();
    let wanted = |name: &str| only.as_deref().is_none_or(|want| want == name);

    if wanted("python") {
        results.extend(python::measure(&plan)?);
    }
    if wanted("typescript") {
        results.extend(typescript::measure(&plan)?);
    }
    if wanted("svelte") {
        results.extend(svelte::measure(&plan)?);
    }
    if wanted("csharp") {
        results.extend(csharp::measure(&plan)?);
    }
    if wanted("rust") {
        results.extend(rust::measure(&plan)?);
    }
    if only.is_none() {
        results.extend(fleet::measure(&plan)?);
    }

    if let Some(output_path) = output_path {
        write_results(&repo_root.join(output_path), &results)?;
    }
    let _ = fs::remove_dir_all(&scratch);

    for result in &results {
        println!(
            "{:<44} elapsed_ms={:<8} success={}",
            result.variant, result.elapsed_ms, result.success
        );
        println!("    {}", result.notes.as_deref().unwrap_or(""));
    }
    if results.iter().all(|result| result.success) {
        Ok(())
    } else {
        Err("a measured phase did not meet its own acceptance".into())
    }
}

struct Plan {
    repo_root: PathBuf,
    scratch: PathBuf,
    warm_repeat: usize,
    cold_samples: usize,
    lifecycle_repeat: usize,
}

impl Plan {
    fn spike(&self, name: &str) -> PathBuf {
        self.repo_root.join("scripts").join(name)
    }

    fn fixture(&self, name: &str) -> PathBuf {
        self.repo_root
            .join("fixtures")
            .join("workspaces")
            .join(name)
    }
}

// ---------------------------------------------------------------------
// One family's measured slice
// ---------------------------------------------------------------------

/// An indexed copy of one fixture, with nothing started yet.
struct Slice {
    workspace: PathBuf,
    db_path: PathBuf,
}

impl Slice {
    fn open(plan: &Plan, label: &str, fixture: &str, link_modules: bool) -> Result<Self, Failure> {
        let base = plan.scratch.join(label);
        let _ = fs::remove_dir_all(&base);
        let workspace = base.join("workspace");
        copy_tree(&plan.fixture(fixture), &workspace)?;
        if link_modules {
            // The *pinned* dependency tree, linked rather than copied:
            // the same arrangement the acceptance suites measure, and
            // the reason "dependencies are not deep-indexed" is a claim
            // about a real `node_modules`.
            let pinned = plan
                .spike(&format!("{}_semantic_spike", label))
                .join("node_modules")
                .canonicalize()?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&pinned, workspace.join("node_modules"))?;
            #[cfg(not(unix))]
            copy_tree(&pinned, &workspace.join("node_modules"))?;
        }
        let db_path = base.join("data").join("index.db");
        BaselineScan::open(&db_path)?.run_initial_scan(
            &workspace,
            &WorkspaceConfig::default(),
            "bench-rev-1",
        )?;
        Ok(Self { workspace, db_path })
    }

    fn rescan(&self, revision: &str) -> Result<(), Failure> {
        BaselineScan::open(&self.db_path)?.run_initial_scan(
            &self.workspace,
            &WorkspaceConfig::default(),
            revision,
        )?;
        Ok(())
    }

    fn resources(&self) -> Result<Vec<Resource>, Failure> {
        Ok(ResourceStore::open(&self.db_path)?.list_active()?)
    }

    /// Every active Resource whose path ends in one of `extensions`.
    fn sources(&self, extensions: &[&str]) -> Result<Vec<Resource>, Failure> {
        let mut found: Vec<Resource> = self
            .resources()?
            .into_iter()
            .filter(|resource| {
                extensions
                    .iter()
                    .any(|extension| resource.path_key.ends_with(extension))
            })
            .collect();
        found.sort_by(|left, right| left.path_key.cmp(&right.path_key));
        Ok(found)
    }

    fn resource(&self, rel: &str) -> Result<Resource, Failure> {
        self.resources()?
            .into_iter()
            .find(|resource| resource.path_key == rel)
            .ok_or_else(|| format!("{rel} is not indexed").into())
    }

    fn db_bytes(&self) -> u64 {
        fs::metadata(&self.db_path)
            .map(|meta| meta.len())
            .unwrap_or(0)
    }

    fn semantic(&self) -> Result<SemanticIndex, Failure> {
        Ok(SemanticIndex::open(&self.db_path)?)
    }
}

// ---------------------------------------------------------------------
// The proof census, per tier
// ---------------------------------------------------------------------

/// The reasons the structural tier classifies as needing a backend.
/// Spelled out rather than imported, so this counts exactly what it says.
const REQUIRES_SEMANTICS: &str = "'MODULE_TREE_REQUIRES_SEMANTICS','NAMESPACE_REQUIRES_SEMANTICS',\
     'RECEIVER_TYPE_REQUIRED','TYPE_SEMANTICS_REQUIRED',\
     'OVERRIDE_TARGET_REQUIRES_SEMANTICS','CONFIG_DEPENDENT_SPECIFIER'";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Accuracy {
    confirmed: i64,
    candidates: i64,
    unresolved: i64,
    requires_semantics: i64,
}

fn accuracy(db_path: &Path) -> Result<Accuracy, Failure> {
    let connection = Connection::open(db_path)?;
    let count = |sql: String| -> Result<i64, Failure> {
        Ok(connection.query_row(&sql, [], |row| row.get(0))?)
    };
    Ok(Accuracy {
        confirmed: count("SELECT COUNT(*) FROM relation".to_owned())?,
        candidates: count("SELECT COUNT(*) FROM relation_candidate".to_owned())?,
        unresolved: count("SELECT COUNT(*) FROM unresolved_reference".to_owned())?,
        requires_semantics: count(format!(
            "SELECT COUNT(*) FROM unresolved_reference WHERE reason IN ({REQUIRES_SEMANTICS})"
        ))?,
    })
}

/// What the semantic tier actually persisted, in rows.
///
/// The `index.db` file size is a coarse instrument at fixture scale --
/// SQLite hands out pages ahead of need, so a few dozen rows land
/// inside pages that were already allocated and the file does not move
/// at all. The row counts say what was stored; the file size says what
/// it cost on disk. Both are reported, neither is dressed up.
fn persisted_rows(db_path: &Path) -> Result<(i64, i64, i64), Failure> {
    let connection = Connection::open(db_path)?;
    let count = |table: &str| -> Result<i64, Failure> {
        Ok(
            connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })?,
        )
    };
    Ok((
        count("relation")?,
        count("semantic_evidence")?,
        count("semantic_publication")?,
    ))
}

fn delta_notes(before: Accuracy, after: Accuracy) -> String {
    format!(
        "confirmed={}->{}; candidates={}->{}; unresolved={}->{}; \
         requires_semantics={}->{}; resolved_delta={}",
        before.confirmed,
        after.confirmed,
        before.candidates,
        after.candidates,
        before.unresolved,
        after.unresolved,
        before.requires_semantics,
        after.requires_semantics,
        after.confirmed - before.confirmed
    )
}

// ---------------------------------------------------------------------
// Distributions
// ---------------------------------------------------------------------

/// One phase's observations, reported as a distribution and never as a
/// lucky single timing.
struct Samples {
    micros: Vec<u128>,
}

impl Samples {
    fn new() -> Self {
        Self { micros: Vec::new() }
    }

    fn push(&mut self, elapsed: Duration) {
        self.micros.push(elapsed.as_micros());
    }

    fn sorted(&self) -> Vec<u128> {
        let mut values = self.micros.clone();
        values.sort_unstable();
        values
    }

    fn median_ms(&self) -> u64 {
        let values = self.sorted();
        if values.is_empty() {
            return 0;
        }
        u64::try_from(values[values.len() / 2] / 1000).unwrap_or(u64::MAX)
    }

    /// `p95` only where the sample count can place one: with fewer than
    /// 20 samples the 95th percentile is the maximum wearing a label it
    /// has not earned.
    fn notes(&self, label: &str) -> String {
        let values = self.sorted();
        let n = values.len();
        if n == 0 {
            return format!("{label}_samples=0");
        }
        let p = |q: usize| -> String {
            let index = (n * q / 100).min(n - 1);
            format!("{:.3}", values[index] as f64 / 1000.0)
        };
        let p95 = if n >= 20 {
            format!("; {label}_p95_ms={}", p(95))
        } else {
            format!("; {label}_p95_ms=not_measured(samples<20)")
        };
        let min = values[0] as f64 / 1000.0;
        let max = values[n - 1] as f64 / 1000.0;
        format!(
            "{label}_samples={n}; {label}_min_ms={min:.3}; {label}_p50_ms={}{p95}; \
             {label}_max_ms={max:.3}; {label}_observations_ms={}",
            p(50),
            values
                .iter()
                .map(|micros| format!("{:.3}", *micros as f64 / 1000.0))
                .collect::<Vec<_>>()
                .join("+")
        )
    }
}

// ---------------------------------------------------------------------
// The Agent-facing warm query, with no backend running
// ---------------------------------------------------------------------

use brainprint_engine::{
    graph::{GraphEndpoint, RelationKind},
    prepare::InspectPreparer,
    query::{QueryIndex, SymbolQuery, SymbolSelector},
    relations::{Direction, RelationIndex},
};

struct WarmQuery {
    tool_calls: u64,
    prepared_bytes: u64,
    prepared_ranges: usize,
    raw_bytes: u64,
    duplicate_bytes: u64,
    references: usize,
    source_complete: bool,
}

/// *What is this, who reaches it, and what source do I need?* -- through
/// the ordinary I4 query surfaces, over persisted canonical truth.
fn warm_query(slice: &Slice, symbol_name: &str) -> Result<WarmQuery, Failure> {
    // Call 1: locate the Symbol by name.
    let index = QueryIndex::open(&slice.db_path)?;
    let located = index.search_symbols(&SymbolQuery::new(SymbolSelector::Name(symbol_name)))?;
    let definition = located
        .candidates
        .first()
        .ok_or_else(|| format!("{symbol_name} is not in the index"))?;
    let target = GraphEndpoint::Symbol(definition.symbol.id);

    // Call 2: who reaches it, as confirmed relations.
    let relations = RelationIndex::open(&slice.db_path)?;
    let incoming = relations.incoming(
        &target,
        &[
            RelationKind::Calls,
            RelationKind::References,
            RelationKind::Implements,
        ],
    )?;

    // Call 3: one prepared inspection -- the evidence spans and the
    // current source around them.
    let preparer = InspectPreparer::open(&slice.db_path, &slice.workspace)?;
    let prepared = preparer.prepare(
        &target,
        Direction::Incoming,
        &[RelationKind::Calls, RelationKind::References],
    )?;

    // `ranges` is already the distinct set the preparer read, so a
    // duplicate here would be the preparer handing the same bytes
    // twice -- counted rather than assumed away.
    let mut seen: BTreeMap<String, BTreeSet<(usize, usize)>> = BTreeMap::new();
    let mut prepared_bytes = 0_u64;
    let mut prepared_ranges = 0_usize;
    let mut duplicate_bytes = 0_u64;
    for range in &prepared.ranges {
        prepared_ranges += 1;
        let bytes = range.source.len() as u64;
        prepared_bytes += bytes;
        let span = (range.span.start_byte, range.span.end_byte);
        if !seen.entry(range.path_rel.clone()).or_default().insert(span) {
            duplicate_bytes += bytes;
        }
    }
    // What reading the same files whole would have cost instead.
    let mut raw_bytes = 0_u64;
    for path_rel in seen.keys() {
        raw_bytes += fs::metadata(slice.workspace.join(path_rel))
            .map(|meta| meta.len())
            .unwrap_or(0);
    }

    Ok(WarmQuery {
        tool_calls: 3,
        prepared_bytes,
        prepared_ranges,
        raw_bytes,
        duplicate_bytes,
        references: incoming.confirmed_count(),
        source_complete: prepared.source_complete(),
    })
}

// ---------------------------------------------------------------------
// Result plumbing
// ---------------------------------------------------------------------

fn result(
    variant: &str,
    elapsed_ms: u64,
    metrics: BenchmarkMetrics,
    success: bool,
    notes: &str,
) -> BenchmarkResult {
    let mut recorder = BenchmarkRecorder::new(
        format!("i4-final-{}", process::id()),
        SCENARIO,
        variant.to_owned(),
    );
    recorder.set_tool_calls(metrics.tool_calls.unwrap_or(0));
    recorder.set_source_reads(
        metrics.source_read_bytes.unwrap_or(0),
        metrics.source_read_lines.unwrap_or(0),
        metrics.duplicate_read_bytes.unwrap_or(0),
    );
    // Left unknown on purpose: this process cannot read its own or a
    // child's usage without `unsafe_code`. See the module docs.
    recorder.set_process_usage(None, None);
    let mut finished = recorder.finish(success, None, Some(notes.to_owned()));
    finished.elapsed_ms = elapsed_ms;
    finished.metrics.tool_calls = metrics.tool_calls;
    finished.metrics.source_read_bytes = metrics.source_read_bytes;
    finished.metrics.source_read_lines = metrics.source_read_lines;
    finished.metrics.duplicate_read_bytes = metrics.duplicate_read_bytes;
    finished
}

/// A line for a family whose toolchain is not installed. Recorded rather
/// than omitted: a skipped measurement is a fact about this machine.
fn skipped(variant: &str, reason: &str) -> BenchmarkResult {
    result(
        variant,
        0,
        BenchmarkMetrics {
            tool_calls: None,
            source_read_bytes: None,
            source_read_lines: None,
            duplicate_read_bytes: None,
            process_cpu_ms: None,
            peak_rss_bytes: None,
        },
        true,
        &format!("skipped=backend_not_installed; reason={reason}; every metric not_measured"),
    )
}

fn write_results(path: &Path, results: &[BenchmarkResult]) -> Result<(), Failure> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut body = String::new();
    for result in results {
        body.push_str(&result.to_json_line()?);
    }
    fs::write(path, body)?;
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), Failure> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "target" || name == "node_modules" || name == "build-rs-ran.marker" {
            continue;
        }
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn ms(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// The metrics shape a phase that read no source carries.
fn no_source(tool_calls: u64) -> BenchmarkMetrics {
    BenchmarkMetrics {
        tool_calls: Some(tool_calls),
        source_read_bytes: Some(0),
        source_read_lines: Some(0),
        duplicate_read_bytes: Some(0),
        process_cpu_ms: None,
        peak_rss_bytes: None,
    }
}

// ---------------------------------------------------------------------
// Per-family measurement
// ---------------------------------------------------------------------

/// One family's measured phases, assembled in the order the metrics
/// depend on each other: cold starts first, then one live host for the
/// refresh and the freshness transitions, then the warm query with
/// every backend shut down.
struct Phases {
    family: &'static str,
    cold: Samples,
    first_refresh: Duration,
    owners: usize,
    evidence: usize,
    before: Accuracy,
    after: Accuracy,
    db_before: u64,
    db_after: u64,
    rows: (i64, i64, i64),
    save: Samples,
    /// Owners that read CURRENT while their source had already moved.
    /// The hard gate: this must stay 0.
    stale_current: usize,
    config: Option<Samples>,
    config_note: String,
    warm: Samples,
    warm_symbol: String,
    warm_query: Option<WarmQuery>,
    backend_starts: usize,
    backend_restarts: usize,
}

impl Phases {
    fn new(family: &'static str) -> Self {
        Self {
            family,
            cold: Samples::new(),
            first_refresh: Duration::ZERO,
            owners: 0,
            evidence: 0,
            before: Accuracy {
                confirmed: 0,
                candidates: 0,
                unresolved: 0,
                requires_semantics: 0,
            },
            after: Accuracy {
                confirmed: 0,
                candidates: 0,
                unresolved: 0,
                requires_semantics: 0,
            },
            db_before: 0,
            db_after: 0,
            rows: (0, 0, 0),
            save: Samples::new(),
            stale_current: 0,
            config: None,
            config_note: "config_to_current=not_measured".to_owned(),
            warm: Samples::new(),
            warm_symbol: String::new(),
            warm_query: None,
            backend_starts: 0,
            backend_restarts: 0,
        }
    }

    fn into_results(self) -> Vec<BenchmarkResult> {
        let family = self.family;
        let mut lines = Vec::new();

        lines.push(result(
            &format!("i4-final-{family}-cold"),
            self.cold.median_ms(),
            no_source(self.backend_starts as u64),
            true,
            &format!(
                "{}; backend_starts={}; backend_restarts={}; \
                 process_cpu_ms=not_measured; peak_rss_bytes=not_measured(see measure_rss.py)",
                self.cold.notes("cold"),
                self.backend_starts,
                self.backend_restarts
            ),
        ));

        lines.push(result(
            &format!("i4-final-{family}-first-refresh"),
            ms(self.first_refresh),
            no_source(self.owners as u64),
            self.evidence > 0,
            &format!(
                "first_refresh_samples=1; owners={}; semantic_evidence={}; {}; \
                 index_db_bytes={}->{}; index_db_delta_bytes={}; \
                 relation_rows={}; semantic_evidence_rows={}; semantic_publication_rows={}",
                self.owners,
                self.evidence,
                delta_notes(self.before, self.after),
                self.db_before,
                self.db_after,
                self.db_after as i64 - self.db_before as i64,
                self.rows.0,
                self.rows.1,
                self.rows.2
            ),
        ));

        lines.push(result(
            &format!("i4-final-{family}-save"),
            self.save.median_ms(),
            no_source(1),
            self.stale_current == 0,
            &format!(
                "{}; stale_current_incidents={}; \
                 transition=edit->structural_rescan->semantic_refresh->CURRENT",
                self.save.notes("save"),
                self.stale_current
            ),
        ));

        let (config_ms, config_notes) = match &self.config {
            Some(samples) => (samples.median_ms(), samples.notes("config")),
            None => (0, self.config_note.clone()),
        };
        lines.push(result(
            &format!("i4-final-{family}-config"),
            config_ms,
            no_source(1),
            true,
            &config_notes,
        ));

        let warm_metrics = self.warm_query.as_ref().map_or_else(
            || no_source(0),
            |query| BenchmarkMetrics {
                tool_calls: Some(query.tool_calls),
                source_read_bytes: Some(query.prepared_bytes),
                source_read_lines: Some(0),
                duplicate_read_bytes: Some(query.duplicate_bytes),
                process_cpu_ms: None,
                peak_rss_bytes: None,
            },
        );
        let warm_notes = self.warm_query.as_ref().map_or_else(
            || "warm_query=not_measured".to_owned(),
            |query| {
                format!(
                    "{}; backend_processes=0; anchor={}; tool_calls={}; references={}; \
                     prepared_ranges={}; prepared_source_bytes={}; \
                     whole_file_bytes_avoided={}; duplicate_source_bytes={}; \
                     source_complete={}; model_input_tokens=not_measured; \
                     model_output_tokens=not_measured",
                    self.warm.notes("warm"),
                    self.warm_symbol,
                    query.tool_calls,
                    query.references,
                    query.prepared_ranges,
                    query.prepared_bytes,
                    query.raw_bytes.saturating_sub(query.prepared_bytes),
                    query.duplicate_bytes,
                    query.source_complete
                )
            },
        );
        lines.push(result(
            &format!("i4-final-{family}-warm"),
            self.warm.median_ms(),
            warm_metrics,
            self.warm_query.is_some(),
            &warm_notes,
        ));

        lines
    }
}

/// The shared tail of every family: the warm phase, run with the
/// backend already shut down.
fn measure_warm(
    plan: &Plan,
    slice: &Slice,
    phases: &mut Phases,
    candidates: &[&str],
) -> Result<(), Failure> {
    // The first candidate that actually has incoming edges. Measuring a
    // symbol nobody reaches would report `references=0`, which reads
    // exactly like the false zero this whole tier exists to avoid --
    // so the fixture's choice of anchor is made explicit instead.
    let mut chosen = None;
    for name in candidates {
        match warm_query(slice, name) {
            Ok(observed) if observed.references > 0 => {
                chosen = Some(*name);
                break;
            }
            Ok(_) if chosen.is_none() => chosen = Some(*name),
            _ => {}
        }
    }
    let symbol = chosen
        .ok_or_else(|| format!("{}: none of {candidates:?} is in the index", phases.family))?;
    phases.warm_symbol = symbol.to_owned();

    // One untimed pass, so the page cache is not what is measured.
    let _ = warm_query(slice, symbol)?;
    for _ in 0..plan.warm_repeat {
        let started = Instant::now();
        let observed = warm_query(slice, symbol)?;
        phases.warm.push(started.elapsed());
        phases.warm_query = Some(observed);
    }
    Ok(())
}

/// The owners one family publishes for.
fn owners_of(
    slice: &Slice,
    context: &AnalysisContext,
    sources: &[Resource],
) -> BTreeSet<SemanticOwner> {
    let _ = slice;
    sources
        .iter()
        .map(|resource| SemanticOwner::new(context.context_key(), resource.id))
        .collect()
}

/// Append a harmless comment, so the file's bytes move without its
/// meaning moving. The transition being measured is freshness, not
/// re-analysis of different code.
fn touch_source(slice: &Slice, rel: &str, comment: &str, round: usize) -> Result<(), Failure> {
    let path = slice.workspace.join(rel);
    let mut text = fs::read_to_string(&path)?;
    text.push_str(&format!("\n{comment} brainprint bench {round}\n"));
    fs::write(&path, text)?;
    Ok(())
}

// ---------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------

mod python {
    use super::*;
    use brainprint_engine::python_semantic::{
        BatchPolicy, PyrightHost, PyrightInstall, PythonLauncher, PythonQueries, PythonSettings,
        RefreshRequest, capability_report, lifecycle,
        protocol::{PythonRequest, PythonResponse},
        refresh_resource, toolchain_identity,
    };

    struct Q<'a> {
        host: &'a PyrightHost,
        cancel: CancelToken,
    }

    impl PythonQueries for Q<'_> {
        fn call(&self, request: &PythonRequest) -> Result<PythonResponse, RequestFailure> {
            self.host
                .call(request, &self.cancel)
                .map_err(RequestFailure::Backend)
        }
    }

    pub fn measure(plan: &Plan) -> Result<Vec<BenchmarkResult>, Failure> {
        let root = plan.spike("python_semantic_spike");
        let Ok(install) = PyrightInstall::locate(&root, "node") else {
            return Ok(vec![skipped(
                "i4-final-python-cold",
                &format!("no pinned pyright install under {}", root.display()),
            )]);
        };
        let slice = Slice::open(plan, "python", "python-semantic-spike", false)?;
        let settings = PythonSettings::default();
        let environment = lifecycle::environment_identity(&slice.workspace, &settings);
        let context = AnalysisContext {
            workspace: WorkspaceId::from_bytes([151; 16]),
            backend: SemanticBackendKind::Python,
            language: ResourceLanguage::Python,
            project_root: ProjectRootIdentity::Key("i4-final-python".to_owned()),
            toolchain: toolchain_identity(&install, &environment),
        };
        let binding = AnalysisContextBinding {
            context: context.clone(),
            project_root_rel: String::new(),
            config_file_rel: Some("pyrightconfig.json".to_owned()),
        };
        let launcher = PythonLauncher::new(install, slice.workspace.clone(), settings.clone());

        let mut phases = Phases::new("python");
        phases.before = accuracy(&slice.db_path)?;
        phases.db_before = slice.db_bytes();

        for _ in 0..plan.cold_samples {
            let started = Instant::now();
            let host = launcher.start(&binding)?;
            phases.cold.push(started.elapsed());
            phases.backend_starts += 1;
            SemanticRuntimeHost::shutdown(&host);
        }

        let host = launcher.start(&binding)?;
        phases.backend_starts += 1;
        let queries = Q {
            host: &host,
            cancel: CancelToken::new(),
        };
        let sources = slice.sources(&[".py"])?;
        let owners = owners_of(&slice, &context, &sources);
        phases.owners = owners.len();

        let index = slice.semantic()?;
        let discovered = lifecycle::discover_config(index.connection(), &slice.workspace, "")?;
        let config = discovered.basis(&settings);
        let capabilities = capability_report(&context);

        let started = Instant::now();
        for resource in &sources {
            let outcome = refresh_resource(
                &index,
                &queries,
                &RefreshRequest {
                    context: &context,
                    workspace_root: &slice.workspace,
                    owner: resource.id,
                    config: &config,
                    capabilities: &capabilities,
                    policy: BatchPolicy { max_attempts: 12 },
                },
            )?;
            phases.evidence += outcome.evidence_count;
        }
        phases.first_refresh = started.elapsed();
        phases.after = accuracy(&slice.db_path)?;
        phases.db_after = slice.db_bytes();
        phases.rows = persisted_rows(&slice.db_path)?;

        // ---- save -> current --------------------------------------
        //
        // The documented order, not a whole-Workspace rescan: withdraw
        // the affected semantic contributions, write, reconcile
        // incrementally, tell the backend, then refresh. Withdrawal
        // comes first because `semantic_evidence.relation_id` has no
        // cascade, and a structural replacement that runs first leaves
        // evidence pointing at relations that are gone.
        let target = sources.first().ok_or("no python source")?.path_key.clone();
        for round in 0..plan.lifecycle_repeat {
            let changed = slice.resource(&target)?;
            let changes = vec![lifecycle::ResourceChange::new(
                changed.id,
                lifecycle::ChangeKind::Changed,
                target.clone(),
            )];
            let started = Instant::now();
            let plan_for_change = lifecycle::plan_changes(&index, &context, &changes, &discovered)?;
            lifecycle::withdraw_affected(
                &index,
                &plan_for_change.affected,
                "SEMANTIC_SOURCE_MOVED",
            )?;
            touch_source(&slice, &target, "#", round)?;
            Reconcile::open(&slice.db_path)?.run(&slice.workspace, &WorkspaceConfig::default())?;
            brainprint_engine::python_semantic::adapter::notify_watched_files(
                &queries,
                lifecycle::watched_changes(&slice.workspace, &changes),
            )?;

            // The stale gate: between the withdrawal and the refresh,
            // nothing affected may still be claiming CURRENT.
            for owner in &plan_for_change.affected {
                if index.status(owner)?.state == SemanticState::Current {
                    phases.stale_current += 1;
                }
            }

            for owner in &plan_for_change.affected {
                refresh_resource(
                    &index,
                    &queries,
                    &RefreshRequest {
                        context: &context,
                        workspace_root: &slice.workspace,
                        owner: owner.owner,
                        config: &config,
                        capabilities: &capabilities,
                        policy: BatchPolicy { max_attempts: 12 },
                    },
                )?;
            }
            phases.save.push(started.elapsed());
            let moved = slice.resource(&target)?;
            let owner = SemanticOwner::new(context.context_key(), moved.id);
            if index.status(&owner)?.state != SemanticState::Current {
                return Err("python: an owner did not reach CURRENT after its refresh".into());
            }
        }

        // ---- config -> current ------------------------------------
        let mut config_samples = Samples::new();
        for round in 0..plan.lifecycle_repeat {
            let started = Instant::now();
            let affected = lifecycle::invalidate_for_config(&index, &context)?;
            lifecycle::withdraw_affected(&index, &affected, "SEMANTIC_CONFIG_CHANGED")?;
            fs::write(
                slice.workspace.join("pyrightconfig.json"),
                format!(
                    "{{\n  \"include\": [\"src\"],\n  \"typeCheckingMode\": \"{}\"\n}}\n",
                    if round % 2 == 0 { "standard" } else { "basic" }
                ),
            )?;
            Reconcile::open(&slice.db_path)?.run(&slice.workspace, &WorkspaceConfig::default())?;
            for owner in &affected {
                if index.status(owner)?.state == SemanticState::Current {
                    phases.stale_current += 1;
                }
            }
            let discovered = lifecycle::discover_config(index.connection(), &slice.workspace, "")?;
            let config = discovered.basis(&settings);
            for owner in &affected {
                refresh_resource(
                    &index,
                    &queries,
                    &RefreshRequest {
                        context: &context,
                        workspace_root: &slice.workspace,
                        owner: owner.owner,
                        config: &config,
                        capabilities: &capabilities,
                        policy: BatchPolicy { max_attempts: 12 },
                    },
                )?;
            }
            config_samples.push(started.elapsed());
        }
        phases.config = Some(config_samples);

        SemanticRuntimeHost::shutdown(&host);
        drop(queries);

        // ---- warm, with nothing running ---------------------------
        measure_warm(
            plan,
            &slice,
            &mut phases,
            &["Base.run", "Base", "render_user", "go"],
        )?;
        Ok(phases.into_results())
    }
}

// ---------------------------------------------------------------------
// TypeScript / JavaScript (React rides on this one)
// ---------------------------------------------------------------------

mod typescript {
    use super::*;
    use brainprint_engine::typescript_semantic::{
        RefreshRequest, TypeScriptHost, TypeScriptInstall, TypeScriptLauncher, TypeScriptQueries,
        capability_report, lifecycle,
        protocol::{TypeScriptRequest, TypeScriptResponse},
        refresh_resource, toolchain_identity,
    };

    struct Q<'a> {
        host: &'a TypeScriptHost,
        cancel: CancelToken,
    }

    impl TypeScriptQueries for Q<'_> {
        fn call(&self, request: &TypeScriptRequest) -> Result<TypeScriptResponse, RequestFailure> {
            self.host
                .call(request, &self.cancel)
                .map_err(RequestFailure::Backend)
        }
    }

    pub fn measure(plan: &Plan) -> Result<Vec<BenchmarkResult>, Failure> {
        let root = plan.spike("typescript_semantic_spike");
        let Ok(install) = TypeScriptInstall::locate(&root) else {
            return Ok(vec![skipped(
                "i4-final-typescript-cold",
                &format!("no pinned typescript install under {}", root.display()),
            )]);
        };
        let slice = Slice::open(plan, "typescript", "typescript-semantic-spike", true)?;
        let index = slice.semantic()?;
        let environment = lifecycle::environment_identity(
            index.connection(),
            &slice.workspace,
            "",
            &install.manifest_version,
        )?;
        let context = AnalysisContext {
            workspace: WorkspaceId::from_bytes([152; 16]),
            backend: SemanticBackendKind::TypeScriptJavaScript,
            language: ResourceLanguage::TypeScript,
            project_root: ProjectRootIdentity::Key("i4-final-typescript".to_owned()),
            toolchain: toolchain_identity(&install, &environment),
        };
        let binding = AnalysisContextBinding {
            context: context.clone(),
            project_root_rel: slice.workspace.to_string_lossy().into_owned(),
            config_file_rel: Some("tsconfig.json".to_owned()),
        };
        let launcher = TypeScriptLauncher::new(install);

        let mut phases = Phases::new("typescript");
        phases.before = accuracy(&slice.db_path)?;
        phases.db_before = slice.db_bytes();

        for _ in 0..plan.cold_samples {
            let started = Instant::now();
            let host = launcher.start(&binding)?;
            phases.cold.push(started.elapsed());
            phases.backend_starts += 1;
            SemanticRuntimeHost::shutdown(&host);
        }

        let host = launcher.start(&binding)?;
        phases.backend_starts += 1;
        let encoding = host.encoding();
        let queries = Q {
            host: &host,
            cancel: CancelToken::new(),
        };

        let discovered = lifecycle::discover_config(index.connection(), &slice.workspace, "")?;
        let config = discovered.basis();
        let capabilities = capability_report(&context);
        let served: Vec<Resource> = slice
            .resources()?
            .into_iter()
            .filter(|resource| {
                resource
                    .language
                    .is_some_and(|language| lifecycle::SERVED_LANGUAGES.contains(&language))
            })
            .collect();
        phases.owners = served.len();

        let started = Instant::now();
        for resource in &served {
            let outcome = refresh_resource(
                &index,
                &queries,
                &RefreshRequest {
                    context: &context,
                    workspace_root: &slice.workspace,
                    owner: resource.id,
                    config: &config,
                    capabilities: &capabilities,
                    encoding,
                },
            )?;
            phases.evidence += outcome.evidence_count;
        }
        phases.first_refresh = started.elapsed();
        phases.after = accuracy(&slice.db_path)?;
        phases.db_after = slice.db_bytes();
        phases.rows = persisted_rows(&slice.db_path)?;

        let target = served
            .iter()
            .find(|resource| resource.path_key == "src/consumer.ts")
            .or_else(|| served.first())
            .ok_or("no typescript source")?
            .path_key
            .clone();
        for round in 0..plan.lifecycle_repeat {
            let changed = slice.resource(&target)?;
            let changes = vec![lifecycle::ResourceChange::new(
                changed.id,
                lifecycle::ChangeKind::Changed,
                target.clone(),
            )];
            let started = Instant::now();
            let change_plan = lifecycle::plan_changes(&index, &context, &changes, &discovered)?;
            lifecycle::withdraw_affected(&index, &change_plan.affected, "SEMANTIC_SOURCE_MOVED")?;
            touch_source(&slice, &target, "//", round)?;
            Reconcile::open(&slice.db_path)?.run(&slice.workspace, &WorkspaceConfig::default())?;
            lifecycle::synchronize(&queries, &slice.workspace, &changes)?;
            for owner in &change_plan.affected {
                if index.status(owner)?.state == SemanticState::Current {
                    phases.stale_current += 1;
                }
            }
            for owner in &change_plan.affected {
                refresh_resource(
                    &index,
                    &queries,
                    &RefreshRequest {
                        context: &context,
                        workspace_root: &slice.workspace,
                        owner: owner.owner,
                        config: &config,
                        capabilities: &capabilities,
                        encoding,
                    },
                )?;
            }
            phases.save.push(started.elapsed());
        }

        // ---- config -> current: the path alias map itself moves ----
        let mut config_samples = Samples::new();
        for round in 0..plan.lifecycle_repeat {
            let started = Instant::now();
            let affected = lifecycle::invalidate_for_config(&index, &context)?;
            lifecycle::withdraw_affected(&index, &affected, "SEMANTIC_CONFIG_CHANGED")?;
            let config_path = slice.workspace.join("tsconfig.json");
            let text = fs::read_to_string(&config_path)?;
            fs::write(&config_path, format!("{text}\n// bench {round}\n"))?;
            Reconcile::open(&slice.db_path)?.run(&slice.workspace, &WorkspaceConfig::default())?;
            for owner in &affected {
                if index.status(owner)?.state == SemanticState::Current {
                    phases.stale_current += 1;
                }
            }
            let discovered = lifecycle::discover_config(index.connection(), &slice.workspace, "")?;
            let config = discovered.basis();
            for owner in &affected {
                refresh_resource(
                    &index,
                    &queries,
                    &RefreshRequest {
                        context: &context,
                        workspace_root: &slice.workspace,
                        owner: owner.owner,
                        config: &config,
                        capabilities: &capabilities,
                        encoding,
                    },
                )?;
            }
            config_samples.push(started.elapsed());
        }
        phases.config = Some(config_samples);

        SemanticRuntimeHost::shutdown(&host);
        drop(queries);

        measure_warm(
            plan,
            &slice,
            &mut phases,
            &["UserCard", "PublicThing", "Thing", "helper"],
        )?;

        // React is not a backend. The TSX component above was answered
        // by this very process, and this line records the count that
        // matters: no sixth server was started for it.
        let mut lines = phases.into_results();
        lines.push(result(
            "i4-final-react",
            0,
            no_source(0),
            true,
            "additional_backend_count=0; served_by=typescript_javascript; \
             tsx=ordinary_typescript; react_specific_capability=none",
        ));
        Ok(lines)
    }
}

// ---------------------------------------------------------------------
// Svelte
// ---------------------------------------------------------------------

mod svelte {
    use super::*;
    use brainprint_engine::svelte_semantic::{
        RefreshRequest, SvelteHost, SvelteInstall, SvelteLauncher, SvelteQueries,
        capability_report, lifecycle,
        protocol::{SvelteRequest, SvelteResponse},
        refresh_resource, toolchain_identity,
    };

    struct Q<'a> {
        host: &'a SvelteHost,
        cancel: CancelToken,
    }

    impl SvelteQueries for Q<'_> {
        fn call(&self, request: &SvelteRequest) -> Result<SvelteResponse, RequestFailure> {
            self.host
                .call(request, &self.cancel)
                .map_err(RequestFailure::Backend)
        }
    }

    pub fn measure(plan: &Plan) -> Result<Vec<BenchmarkResult>, Failure> {
        let root = plan.spike("svelte_semantic_spike");
        let Ok(install) = SvelteInstall::locate(&root) else {
            return Ok(vec![skipped(
                "i4-final-svelte-cold",
                &format!("no pinned svelte language tools under {}", root.display()),
            )]);
        };
        let slice = Slice::open(plan, "svelte", "svelte-semantic-spike", true)?;
        let index = slice.semantic()?;
        let environment =
            lifecycle::environment_identity(index.connection(), &slice.workspace, "", &install)?;
        let context = AnalysisContext {
            workspace: WorkspaceId::from_bytes([153; 16]),
            // A backend kind and language of its own: this is what lets
            // Svelte run its own TypeScript beside the standalone one.
            backend: SemanticBackendKind::Svelte,
            language: ResourceLanguage::Svelte,
            project_root: ProjectRootIdentity::Key("i4-final-svelte".to_owned()),
            toolchain: toolchain_identity(&install, &environment),
        };
        let binding = AnalysisContextBinding {
            context: context.clone(),
            project_root_rel: slice.workspace.to_string_lossy().into_owned(),
            config_file_rel: None,
        };
        let node = env::var("BRAINPRINT_NODE").unwrap_or_else(|_| "node".to_owned());
        let launcher = SvelteLauncher::new(install, node);

        let mut phases = Phases::new("svelte");
        phases.before = accuracy(&slice.db_path)?;
        phases.db_before = slice.db_bytes();

        for _ in 0..plan.cold_samples {
            let started = Instant::now();
            let host = launcher.start(&binding)?;
            phases.cold.push(started.elapsed());
            phases.backend_starts += 1;
            SemanticRuntimeHost::shutdown(&host);
        }

        let host = launcher.start(&binding)?;
        phases.backend_starts += 1;
        let queries = Q {
            host: &host,
            cancel: CancelToken::new(),
        };

        // The barrier: a component the server has never been told about
        // is an error rather than an empty answer.
        lifecycle::announce_components(&index, &queries, &slice.workspace)?;
        let discovered = lifecycle::discover_config(index.connection(), "")?;
        let config = discovered.basis();
        let capabilities = capability_report(&context);
        let components: Vec<Resource> = slice
            .resources()?
            .into_iter()
            .filter(|resource| resource.language == Some(ResourceLanguage::Svelte))
            .collect();
        phases.owners = components.len();

        let started = Instant::now();
        for resource in &components {
            let outcome = refresh_resource(
                &index,
                &queries,
                &RefreshRequest {
                    context: &context,
                    workspace_root: &slice.workspace,
                    owner: resource.id,
                    config: &config,
                    capabilities: &capabilities,
                },
            )?;
            phases.evidence += outcome.evidence_count;
        }
        phases.first_refresh = started.elapsed();
        phases.after = accuracy(&slice.db_path)?;
        phases.db_after = slice.db_bytes();
        phases.rows = persisted_rows(&slice.db_path)?;

        let target = components
            .first()
            .ok_or("no svelte component")?
            .path_key
            .clone();
        for round in 0..plan.lifecycle_repeat {
            let changed = slice.resource(&target)?;
            let changes = vec![lifecycle::ResourceChange::new(
                changed.id,
                lifecycle::ChangeKind::Changed,
                target.clone(),
            )];
            let started = Instant::now();
            let change_plan = lifecycle::plan_changes(&index, &context, &changes, &discovered)?;
            lifecycle::withdraw_affected(&index, &change_plan.affected, "SEMANTIC_SOURCE_MOVED")?;
            touch_source(&slice, &target, "<!--", round)?;
            let path = slice.workspace.join(&target);
            let text = fs::read_to_string(&path)?;
            fs::write(&path, format!("{}-->\n", text.trim_end()))?;
            Reconcile::open(&slice.db_path)?.run(&slice.workspace, &WorkspaceConfig::default())?;
            lifecycle::synchronize(&queries, &slice.workspace, &changes)?;
            for owner in &change_plan.affected {
                if index.status(owner)?.state == SemanticState::Current {
                    phases.stale_current += 1;
                }
            }
            for owner in &change_plan.affected {
                refresh_resource(
                    &index,
                    &queries,
                    &RefreshRequest {
                        context: &context,
                        workspace_root: &slice.workspace,
                        owner: owner.owner,
                        config: &config,
                        capabilities: &capabilities,
                    },
                )?;
            }
            phases.save.push(started.elapsed());
        }
        phases.config_note = "config_to_current=not_measured; reason=the Svelte container's \
             config basis is discovered from the toolchain rather than from one \
             Workspace file this harness can move; the source transition above is \
             the freshness proof for this family"
            .to_owned();

        SemanticRuntimeHost::shutdown(&host);
        drop(queries);

        measure_warm(plan, &slice, &mut phases, &["Child", "count", "label"])?;
        Ok(phases.into_results())
    }
}

// ---------------------------------------------------------------------
// C#
// ---------------------------------------------------------------------

mod csharp {
    use super::*;
    use brainprint_engine::csharp_semantic::{
        CSharpHost, CSharpInstall, CSharpLauncher, CSharpQueries, RefreshRequest,
        capability_report, lifecycle,
        protocol::{CSharpRequest, CSharpResponse, path_to_uri},
        refresh_resource, toolchain_identity,
    };

    const LOAD_TIMEOUT: Duration = Duration::from_secs(180);

    struct Q<'a> {
        host: &'a CSharpHost,
        cancel: CancelToken,
    }

    impl CSharpQueries for Q<'_> {
        fn call(&self, request: &CSharpRequest) -> Result<CSharpResponse, RequestFailure> {
            self.host
                .call(request, &self.cancel)
                .map_err(RequestFailure::Backend)
        }
    }

    impl lifecycle::DocumentVersions for Q<'_> {
        fn next_document_version(&self) -> i64 {
            self.host.next_document_version()
        }

        fn exchange_text(&self, uri: &str, text: &str) -> Option<String> {
            self.host.exchange_text(uri, text)
        }
    }

    impl lifecycle::ProjectLoadBarrier for Q<'_> {
        fn completions_seen(&self) -> u64 {
            self.host.load_completions()
        }

        fn wait_for_project_load(&self, seen: u64) -> Result<usize, String> {
            if self.host.wait_for_project_load(seen, LOAD_TIMEOUT) {
                Ok(1)
            } else {
                Err(format!(
                    "no project load was announced within {LOAD_TIMEOUT:?}"
                ))
            }
        }
    }

    pub fn measure(plan: &Plan) -> Result<Vec<BenchmarkResult>, Failure> {
        let root = plan.spike("csharp_semantic_spike");
        let Ok(install) = CSharpInstall::locate(&root) else {
            return Ok(vec![skipped(
                "i4-final-csharp-cold",
                &format!(
                    "no restored Roslyn language server under {}",
                    root.display()
                ),
            )]);
        };
        let slice = Slice::open(plan, "csharp", "csharp-semantic-spike", false)?;
        // Trusted: the Level A path needs the projects loaded, and the
        // fixture is a committed, reviewed tree. Recorded, not assumed.
        let trust = ProjectExecutionTrust::Trusted;
        restore(&slice.workspace)?;
        slice.rescan("bench-rev-2")?;

        let index = slice.semantic()?;
        let projects =
            lifecycle::discover_projects_under(index.connection(), trust, Some(&slice.workspace))?;
        let environment = lifecycle::environment_identity(&install, &projects)?;
        let context = AnalysisContext {
            workspace: WorkspaceId::from_bytes([154; 16]),
            backend: SemanticBackendKind::CSharp,
            language: ResourceLanguage::CSharp,
            project_root: ProjectRootIdentity::Key("i4-final-csharp".to_owned()),
            toolchain: toolchain_identity(&install, &environment),
        };
        let binding = AnalysisContextBinding {
            context: context.clone(),
            project_root_rel: slice.workspace.to_string_lossy().into_owned(),
            config_file_rel: None,
        };
        let launcher = CSharpLauncher::new(
            install,
            trust,
            plan.scratch.join("csharp").join("server-logs"),
        );

        let mut phases = Phases::new("csharp");
        phases.before = accuracy(&slice.db_path)?;
        phases.db_before = slice.db_bytes();

        // Cold here is spawn + handshake + solution load + the server
        // announcing initialization -- the point at which it can answer.
        for _ in 0..plan.cold_samples {
            let started = Instant::now();
            let host = launcher.start(&binding)?;
            let queries = Q {
                host: &host,
                cancel: CancelToken::new(),
            };
            let seen = host.load_completions();
            queries.call(&CSharpRequest::OpenSolution {
                uri: path_to_uri(&slice.workspace.join("CSharpSemanticSpike.sln")),
            })?;
            if !host.wait_for_project_load(seen, LOAD_TIMEOUT) {
                return Err("csharp: the server never announced project initialization".into());
            }
            phases.cold.push(started.elapsed());
            phases.backend_starts += 1;
            drop(queries);
            SemanticRuntimeHost::shutdown(&host);
        }

        let host = launcher.start(&binding)?;
        phases.backend_starts += 1;
        let queries = Q {
            host: &host,
            cancel: CancelToken::new(),
        };
        let seen = host.load_completions();
        queries.call(&CSharpRequest::OpenSolution {
            uri: path_to_uri(&slice.workspace.join("CSharpSemanticSpike.sln")),
        })?;
        if !host.wait_for_project_load(seen, LOAD_TIMEOUT) {
            return Err("csharp: the server never announced project initialization".into());
        }

        let sources = slice.sources(&[".cs"])?;
        phases.owners = sources.len();
        let handed: Vec<lifecycle::ResourceChange> = sources
            .iter()
            .map(|resource| {
                lifecycle::ResourceChange::new(
                    resource.id,
                    lifecycle::ChangeKind::Changed,
                    resource.path_key.clone(),
                )
            })
            .collect();
        lifecycle::synchronize_documents(&queries, &queries, &slice.workspace, &handed)?;

        let config = projects.basis();
        let capabilities = capability_report(&context, trust);
        let started = Instant::now();
        for resource in &sources {
            let outcome = refresh_resource(
                &index,
                &queries,
                &RefreshRequest {
                    context: &context,
                    workspace_root: &slice.workspace,
                    owner: resource.id,
                    config: &config,
                    capabilities: &capabilities,
                    projects: &projects,
                },
            )?;
            phases.evidence += outcome.evidence_count;
        }
        phases.first_refresh = started.elapsed();
        phases.after = accuracy(&slice.db_path)?;
        phases.db_after = slice.db_bytes();
        phases.rows = persisted_rows(&slice.db_path)?;

        let target = sources.first().ok_or("no C# source")?.path_key.clone();
        for round in 0..plan.lifecycle_repeat {
            let changed = slice.resource(&target)?;
            let changes = vec![lifecycle::ResourceChange::new(
                changed.id,
                lifecycle::ChangeKind::Changed,
                target.clone(),
            )];
            let started = Instant::now();
            let change_plan = lifecycle::plan_changes(&index, &context, &changes, &projects)?;
            lifecycle::withdraw_affected(&index, &change_plan.affected, "SEMANTIC_SOURCE_MOVED")?;
            touch_source(&slice, &target, "//", round)?;
            Reconcile::open(&slice.db_path)?.run(&slice.workspace, &WorkspaceConfig::default())?;
            lifecycle::synchronize_documents(&queries, &queries, &slice.workspace, &changes)?;
            for owner in &change_plan.affected {
                if index.status(owner)?.state == SemanticState::Current {
                    phases.stale_current += 1;
                }
            }
            for owner in &change_plan.affected {
                refresh_resource(
                    &index,
                    &queries,
                    &RefreshRequest {
                        context: &context,
                        workspace_root: &slice.workspace,
                        owner: owner.owner,
                        config: &config,
                        capabilities: &capabilities,
                        projects: &projects,
                    },
                )?;
            }
            phases.save.push(started.elapsed());
        }

        // ---- project change -> current -----------------------------
        //
        // Fewer rounds than a source save: a solution reload is the
        // expensive transition, and repeating it ten times measures
        // MSBuild rather than Brainprint. The lower count is recorded.
        let project_rounds = plan.lifecycle_repeat.min(3);
        let mut config_samples = Samples::new();
        let project_file = slice
            .resources()?
            .into_iter()
            .find(|resource| resource.path_key.ends_with(".csproj"))
            .ok_or("no .csproj")?;
        for round in 0..project_rounds {
            let changes = vec![lifecycle::ResourceChange::new(
                project_file.id,
                lifecycle::ChangeKind::Changed,
                project_file.path_key.clone(),
            )];
            let started = Instant::now();
            let affected = lifecycle::invalidate_for_config(&index, &context)?;
            lifecycle::withdraw_affected(&index, &affected, "SEMANTIC_CONFIG_CHANGED")?;
            let path = slice.workspace.join(&project_file.path_key);
            let text = fs::read_to_string(&path)?;
            fs::write(&path, format!("{text}<!-- bench {round} -->\n"))?;
            Reconcile::open(&slice.db_path)?.run(&slice.workspace, &WorkspaceConfig::default())?;
            lifecycle::reload_projects(&queries, &queries, &slice.workspace, &changes, &projects)?;
            for owner in &affected {
                if index.status(owner)?.state == SemanticState::Current {
                    phases.stale_current += 1;
                }
            }
            for owner in &affected {
                refresh_resource(
                    &index,
                    &queries,
                    &RefreshRequest {
                        context: &context,
                        workspace_root: &slice.workspace,
                        owner: owner.owner,
                        config: &config,
                        capabilities: &capabilities,
                        projects: &projects,
                    },
                )?;
            }
            config_samples.push(started.elapsed());
        }
        phases.config = Some(config_samples);

        drop(queries);
        SemanticRuntimeHost::shutdown(&host);

        measure_warm(
            plan,
            &slice,
            &mut phases,
            &["Core.Runner", "Runner", "Core.IRunner", "IRunner"],
        )?;
        let mut lines = phases.into_results();
        lines.push(result(
            "i4-final-csharp-trust",
            0,
            no_source(0),
            true,
            "measured_mode=Trusted; reason=the Level A path needs the projects loaded; \
             untrusted_mode=Level B, proved by i4_csharp_acceptance; \
             multi_target=PARTIAL, unchanged from task 12",
        ));
        Ok(lines)
    }

    /// `dotnet restore`, once, so the projects load offline. Developer
    /// tooling, exactly as the C# acceptance does it -- production
    /// Brainprint never runs this.
    fn restore(workspace: &Path) -> Result<(), Failure> {
        let output = process::Command::new(
            env::var("BRAINPRINT_DOTNET").unwrap_or_else(|_| "dotnet".to_owned()),
        )
        .args(["restore"])
        .current_dir(workspace)
        .output()?;
        if !output.status.success() {
            return Err(format!(
                "dotnet restore failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------
// Rust
// ---------------------------------------------------------------------

mod rust {
    use super::*;
    use brainprint_engine::rust_semantic::{
        RefreshRequest, RustHost, RustInstall, RustLauncher, RustQueries, capability_report,
        lifecycle,
        protocol::{RustRequest, RustResponse},
        refresh_resource, toolchain_identity,
    };

    const SETTLE_TIMEOUT: Duration = Duration::from_secs(240);

    struct Q<'a> {
        host: &'a RustHost,
        cancel: CancelToken,
    }

    impl RustQueries for Q<'_> {
        fn call(&self, request: &RustRequest) -> Result<RustResponse, RequestFailure> {
            self.host
                .call(request, &self.cancel)
                .map_err(RequestFailure::Backend)
        }
    }

    impl lifecycle::DocumentVersions for Q<'_> {
        fn next_document_version(&self) -> i64 {
            self.host.next_document_version()
        }

        fn exchange_text(&self, uri: &str, text: &str) -> Option<String> {
            self.host.exchange_text(uri, text)
        }
    }

    impl lifecycle::QuiescenceBarrier for Q<'_> {
        fn settlings_seen(&self) -> u64 {
            self.host.load_completions()
        }

        fn wait_for_quiescent(&self, seen: u64) -> Result<usize, String> {
            if self.host.wait_for_quiescent(seen, SETTLE_TIMEOUT) {
                Ok(1)
            } else {
                Err(format!(
                    "the server did not settle within {SETTLE_TIMEOUT:?}"
                ))
            }
        }
    }

    /// The executable, found the one way a *benchmark* may ask.
    /// Production Brainprint is handed the path.
    fn install() -> Option<RustInstall> {
        let which = process::Command::new("rustup")
            .args(["which", "rust-analyzer"])
            .output()
            .ok()?;
        if !which.status.success() {
            return None;
        }
        RustInstall::at(String::from_utf8_lossy(&which.stdout).trim()).ok()
    }

    pub fn measure(plan: &Plan) -> Result<Vec<BenchmarkResult>, Failure> {
        let Some(install) = install() else {
            return Ok(vec![skipped(
                "i4-final-rust-cold",
                "rustup which rust-analyzer found nothing",
            )]);
        };
        let slice = Slice::open(plan, "rust", "rust-semantic-spike", false)?;
        let trust = ProjectExecutionTrust::Trusted;
        let index = slice.semantic()?;
        let packages =
            lifecycle::discover_packages_under(index.connection(), trust, Some(&slice.workspace))?;
        let environment = lifecycle::environment_identity(&install, &packages)?;
        let context = AnalysisContext {
            workspace: WorkspaceId::from_bytes([155; 16]),
            backend: SemanticBackendKind::Rust,
            language: ResourceLanguage::Rust,
            project_root: ProjectRootIdentity::Key("i4-final-rust".to_owned()),
            toolchain: toolchain_identity(&install, &environment),
        };
        let binding = AnalysisContextBinding {
            context: context.clone(),
            project_root_rel: slice.workspace.to_string_lossy().into_owned(),
            config_file_rel: None,
        };
        let marker = slice.workspace.join("crates/core/build-rs-ran.marker");
        let launcher = RustLauncher::new(install, trust);

        let mut phases = Phases::new("rust");
        phases.before = accuracy(&slice.db_path)?;
        phases.db_before = slice.db_bytes();

        // Cold is start + the server announcing it has settled: the
        // point at which the crate graph can answer.
        for _ in 0..plan.cold_samples {
            let started = Instant::now();
            let host = launcher.start(&binding)?;
            if !host.wait_for_quiescent(0, SETTLE_TIMEOUT) {
                return Err("rust: the server never announced that it settled".into());
            }
            phases.cold.push(started.elapsed());
            phases.backend_starts += 1;
            SemanticRuntimeHost::shutdown(&host);
        }

        let host = launcher.start(&binding)?;
        phases.backend_starts += 1;
        if !host.wait_for_quiescent(0, SETTLE_TIMEOUT) {
            return Err("rust: the server never announced that it settled".into());
        }
        let queries = Q {
            host: &host,
            cancel: CancelToken::new(),
        };

        let sources = slice.sources(&[".rs"])?;
        phases.owners = sources.len();
        let handed: Vec<lifecycle::ResourceChange> = sources
            .iter()
            .map(|resource| {
                lifecycle::ResourceChange::new(
                    resource.id,
                    lifecycle::ChangeKind::Changed,
                    resource.path_key.clone(),
                )
            })
            .collect();
        lifecycle::synchronize_documents(&queries, &queries, &slice.workspace, &handed)?;

        let config = packages.basis();
        let capabilities = capability_report(&context, trust);
        let started = Instant::now();
        for resource in &sources {
            let outcome = refresh_resource(
                &index,
                &queries,
                &RefreshRequest {
                    context: &context,
                    workspace_root: &slice.workspace,
                    owner: resource.id,
                    config: &config,
                    capabilities: &capabilities,
                },
            )?;
            phases.evidence += outcome.evidence_count;
        }
        phases.first_refresh = started.elapsed();
        phases.after = accuracy(&slice.db_path)?;
        phases.db_after = slice.db_bytes();
        phases.rows = persisted_rows(&slice.db_path)?;

        let target = sources
            .iter()
            .find(|resource| resource.path_key.ends_with("runner.rs"))
            .or_else(|| sources.first())
            .ok_or("no rust source")?
            .path_key
            .clone();
        for round in 0..plan.lifecycle_repeat {
            let changed = slice.resource(&target)?;
            let changes = vec![lifecycle::ResourceChange::new(
                changed.id,
                lifecycle::ChangeKind::Changed,
                target.clone(),
            )];
            let started = Instant::now();
            let change_plan = lifecycle::plan_changes(&index, &context, &changes, &packages)?;
            lifecycle::withdraw_affected(&index, &change_plan.affected, "SEMANTIC_SOURCE_MOVED")?;
            touch_source(&slice, &target, "//", round)?;
            Reconcile::open(&slice.db_path)?.run(&slice.workspace, &WorkspaceConfig::default())?;
            lifecycle::synchronize_documents(&queries, &queries, &slice.workspace, &changes)?;
            for owner in &change_plan.affected {
                if index.status(owner)?.state == SemanticState::Current {
                    phases.stale_current += 1;
                }
            }
            for owner in &change_plan.affected {
                refresh_resource(
                    &index,
                    &queries,
                    &RefreshRequest {
                        context: &context,
                        workspace_root: &slice.workspace,
                        owner: owner.owner,
                        config: &config,
                        capabilities: &capabilities,
                    },
                )?;
            }
            phases.save.push(started.elapsed());
        }

        // ---- Cargo.toml -> current ---------------------------------
        let manifest_rounds = plan.lifecycle_repeat.min(3);
        let mut config_samples = Samples::new();
        let manifest = slice
            .resources()?
            .into_iter()
            .find(|resource| resource.path_key == "crates/core/Cargo.toml")
            .ok_or("no core manifest")?;
        for round in 0..manifest_rounds {
            let changes = vec![lifecycle::ResourceChange::new(
                manifest.id,
                lifecycle::ChangeKind::Changed,
                manifest.path_key.clone(),
            )];
            let started = Instant::now();
            let affected = lifecycle::invalidate_for_config(&index, &context)?;
            lifecycle::withdraw_affected(&index, &affected, "SEMANTIC_CONFIG_CHANGED")?;
            let path = slice.workspace.join(&manifest.path_key);
            let text = fs::read_to_string(&path)?;
            fs::write(&path, format!("{text}# bench {round}\n"))?;
            Reconcile::open(&slice.db_path)?.run(&slice.workspace, &WorkspaceConfig::default())?;
            lifecycle::reload_projects(&queries, &queries, &slice.workspace, &changes, &packages)?;
            for owner in &affected {
                if index.status(owner)?.state == SemanticState::Current {
                    phases.stale_current += 1;
                }
            }
            for owner in &affected {
                refresh_resource(
                    &index,
                    &queries,
                    &RefreshRequest {
                        context: &context,
                        workspace_root: &slice.workspace,
                        owner: owner.owner,
                        config: &config,
                        capabilities: &capabilities,
                    },
                )?;
            }
            config_samples.push(started.elapsed());
        }
        phases.config = Some(config_samples);

        drop(queries);
        SemanticRuntimeHost::shutdown(&host);

        // The trust assertion, by consequence: nothing in this whole
        // measurement was allowed to execute the project it read.
        let executed = marker.exists() || slice.workspace.join("target").exists();
        measure_warm(
            plan,
            &slice,
            &mut phases,
            &["Runner", "Worker", "Worker::run", "Model"],
        )?;
        let mut lines = phases.into_results();
        lines.push(result(
            "i4-final-rust-environment",
            0,
            no_source(0),
            !executed,
            &format!(
                "measured_mode=Trusted; build_scripts=disabled; proc_macros=disabled; \
                 check_on_save=disabled; cargo_offline=true; build_rs_marker_present={}; \
                 target_dir_created={}; rust_src_present={}",
                marker.exists(),
                slice.workspace.join("target").exists(),
                rust_src_present()
            ),
        ));
        Ok(lines)
    }

    fn rust_src_present() -> bool {
        let Ok(output) = process::Command::new("rustc")
            .args(["--print", "sysroot"])
            .output()
        else {
            return false;
        };
        let sysroot = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Path::new(&sysroot)
            .join("lib/rustlib/src/rust/library/core/src/lib.rs")
            .exists()
    }
}

// ---------------------------------------------------------------------
// The fleet
// ---------------------------------------------------------------------

mod fleet {
    use super::*;

    pub fn measure(plan: &Plan) -> Result<Vec<BenchmarkResult>, Failure> {
        let families = super::installed_fleet(plan)?;
        if families.is_empty() {
            return Ok(vec![skipped(
                "i4-final-fleet",
                "no semantic backend is installed",
            )]);
        }
        let mut supervisor = SemanticRuntimeSupervisor::new(RuntimePolicy::default());
        for family in &families {
            supervisor = supervisor.with_backend(Arc::clone(&family.launcher));
        }

        // Sequential cold start: one supervisor, one context per family,
        // each timed to READY, and the wall clock for all of them.
        let mut per_family = Vec::new();
        let mut leases = Vec::new();
        let whole = Instant::now();
        for family in &families {
            let started = Instant::now();
            let lease = supervisor.acquire(&family.binding)?;
            per_family.push(format!(
                "{}={:.3}",
                family.label,
                started.elapsed().as_secs_f64() * 1000.0
            ));
            leases.push(lease);
        }
        let all_ready = whole.elapsed();

        // The same contexts again, from five more callers: the shared
        // runtime claim, measured rather than asserted.
        let mut extra = Vec::new();
        for family in &families {
            extra.push(supervisor.acquire(&family.binding)?);
        }
        let telemetry = supervisor.fleet_telemetry();
        let live = supervisor.live_runtime_count();
        let duplicate_starts = telemetry
            .starts_succeeded
            .saturating_sub(families.len() as u64);
        let all_ready_state = leases
            .iter()
            .all(|lease| lease.state() == RuntimeState::Ready);
        drop(extra);
        drop(leases);
        supervisor.shutdown();

        Ok(vec![result(
            "i4-final-fleet",
            ms(all_ready),
            no_source(telemetry.starts_succeeded),
            duplicate_starts == 0 && live == families.len() && all_ready_state,
            &format!(
                "families={}; clients={}; contexts={}; live_runtimes={}; \
                 backend_starts={}; duplicate_starts={}; restarts={}; crashes={}; \
                 to_ready_ms={}; all_ready_ms={:.3}; react_additional_backends=0; \
                 known_rss_bytes={:?}; resource_usage_unknown={}; samples=1",
                families
                    .iter()
                    .map(|family| family.label)
                    .collect::<Vec<_>>()
                    .join("+"),
                families.len() * 2,
                telemetry.known_contexts,
                live,
                telemetry.starts_succeeded,
                duplicate_starts,
                telemetry.restart_attempts,
                telemetry.crashes,
                per_family.join("+"),
                all_ready.as_secs_f64() * 1000.0,
                telemetry.known_rss_bytes,
                telemetry.resource_usage_unknown
            ),
        )])
    }
}

/// One installed family, ready to be put in a supervisor.
struct FleetFamily {
    label: &'static str,
    binding: AnalysisContextBinding,
    launcher: Arc<dyn SemanticBackendLauncher>,
}

/// Every family this machine can actually start. A family whose
/// toolchain is absent is left out rather than faked.
fn installed_fleet(plan: &Plan) -> Result<Vec<FleetFamily>, Failure> {
    use brainprint_engine::{
        csharp_semantic::{CSharpInstall, CSharpLauncher},
        python_semantic::{PyrightInstall, PythonLauncher, PythonSettings},
        rust_semantic::{RustInstall, RustLauncher},
        svelte_semantic::{SvelteInstall, SvelteLauncher},
        typescript_semantic::{TypeScriptInstall, TypeScriptLauncher},
    };

    let mut families = Vec::new();
    let mut uid = 200_u8;
    let mut context_for = |label: &'static str,
                           backend: SemanticBackendKind,
                           language: ResourceLanguage,
                           workspace: &Path|
     -> AnalysisContextBinding {
        uid = uid.wrapping_add(1);
        AnalysisContextBinding {
            context: AnalysisContext {
                workspace: WorkspaceId::from_bytes([uid; 16]),
                backend,
                language,
                project_root: ProjectRootIdentity::Key(format!("i4-final-fleet-{label}")),
                toolchain: brainprint_engine::semantic::ToolchainIdentity {
                    backend_version: "fleet".to_owned(),
                    backend_compatibility_class: format!("fleet:{label}"),
                    environment_fingerprint: format!("fleet:{label}"),
                },
            },
            project_root_rel: workspace.to_string_lossy().into_owned(),
            config_file_rel: None,
        }
    };

    if let Ok(install) = PyrightInstall::locate(&plan.spike("python_semantic_spike"), "node") {
        let slice = Slice::open(plan, "fleet-python", "python-semantic-spike", false)?;
        families.push(FleetFamily {
            label: "python",
            binding: context_for(
                "python",
                SemanticBackendKind::Python,
                ResourceLanguage::Python,
                &slice.workspace,
            ),
            launcher: Arc::new(PythonLauncher::new(
                install,
                slice.workspace.clone(),
                PythonSettings::default(),
            )),
        });
    }
    if let Ok(install) = TypeScriptInstall::locate(&plan.spike("typescript_semantic_spike")) {
        let slice = Slice::open(plan, "fleet-typescript", "typescript-semantic-spike", false)?;
        families.push(FleetFamily {
            label: "typescript",
            binding: context_for(
                "typescript",
                SemanticBackendKind::TypeScriptJavaScript,
                ResourceLanguage::TypeScript,
                &slice.workspace,
            ),
            launcher: Arc::new(TypeScriptLauncher::new(install)),
        });
    }
    if let Ok(install) = SvelteInstall::locate(&plan.spike("svelte_semantic_spike")) {
        let slice = Slice::open(plan, "fleet-svelte", "svelte-semantic-spike", false)?;
        families.push(FleetFamily {
            label: "svelte",
            binding: context_for(
                "svelte",
                SemanticBackendKind::Svelte,
                ResourceLanguage::Svelte,
                &slice.workspace,
            ),
            launcher: Arc::new(SvelteLauncher::new(
                install,
                env::var("BRAINPRINT_NODE").unwrap_or_else(|_| "node".to_owned()),
            )),
        });
    }
    if let Ok(install) = CSharpInstall::locate(&plan.spike("csharp_semantic_spike")) {
        let slice = Slice::open(plan, "fleet-csharp", "csharp-semantic-spike", false)?;
        families.push(FleetFamily {
            label: "csharp",
            binding: context_for(
                "csharp",
                SemanticBackendKind::CSharp,
                ResourceLanguage::CSharp,
                &slice.workspace,
            ),
            launcher: Arc::new(CSharpLauncher::new(
                install,
                // Untrusted: the fleet measurement is about process
                // topology, and loading a project is the one thing
                // trust gates.
                ProjectExecutionTrust::Untrusted,
                plan.scratch.join("fleet-csharp").join("server-logs"),
            )),
        });
    }
    if let Some(install) = {
        let which = process::Command::new("rustup")
            .args(["which", "rust-analyzer"])
            .output()
            .ok()
            .filter(|output| output.status.success());
        which
            .and_then(|output| RustInstall::at(String::from_utf8_lossy(&output.stdout).trim()).ok())
    } {
        let slice = Slice::open(plan, "fleet-rust", "rust-semantic-spike", false)?;
        families.push(FleetFamily {
            label: "rust",
            binding: context_for(
                "rust",
                SemanticBackendKind::Rust,
                ResourceLanguage::Rust,
                &slice.workspace,
            ),
            launcher: Arc::new(RustLauncher::new(install, ProjectExecutionTrust::Untrusted)),
        });
    }
    Ok(families)
}

/// Start every installed family in one supervisor and keep them up, so
/// something outside this process can read the process tree. The only
/// way a resource figure gets measured at all: see
/// `scripts/i4_final_acceptance/measure_rss.py`.
fn hold_the_fleet(plan: &Plan, hold_ms: u64) -> Result<(), Failure> {
    let families = installed_fleet(plan)?;
    let mut supervisor = SemanticRuntimeSupervisor::new(RuntimePolicy::default());
    for family in &families {
        supervisor = supervisor.with_backend(Arc::clone(&family.launcher));
    }
    let mut leases = Vec::new();
    for family in &families {
        leases.push(supervisor.acquire(&family.binding)?);
    }
    println!(
        "HOLDING pid={} families={} live={}",
        process::id(),
        families
            .iter()
            .map(|family| family.label)
            .collect::<Vec<_>>()
            .join("+"),
        supervisor.live_runtime_count()
    );
    thread::sleep(Duration::from_millis(hold_ms));
    drop(leases);
    supervisor.shutdown();
    Ok(())
}
