//! Re-measure the I0 representative scenario through I3's Relation
//! Graph (#17 task 15).
//!
//! Benchmark harness, not a product feature. It drives the same
//! `python-signature-impact` fixture and scenario file the I0
//! `basic-tools` baseline and the I2 re-measurement used, and writes
//! the same JSONL schema ([`brainprint_engine::telemetry`]) so the
//! three can be put side by side.
//!
//! ## What is being compared
//!
//! The question is the one an Agent actually has before changing a
//! function's signature: *what is this, who calls it, where exactly,
//! which test covers it, and what source do I need to edit?*
//!
//! - `basic-tools` (I0) answers it with a broad text search and a
//!   re-read of every hit. The hits are *textual*: nothing in that
//!   answer distinguishes the definition from a caller from a test.
//! - `brainprint-i2` answered the definition structurally and left the
//!   rest to one bounded text fallback, again textual.
//! - `brainprint-i3` (this harness) answers all of it from the graph:
//!   a located Symbol, confirmed CALLS with exact evidence spans,
//!   prepared current source for each of them, and a related test
//!   projected from a confirmed relation path. No text search runs.
//!
//! ## Three lines, so the cost added is visible too
//!
//! - `brainprint-i3-index` -- the baseline scan. This is the cost I3
//!   *adds*, and it is measured rather than excluded.
//! - `brainprint-i3` -- the query flow, comparable with the
//!   `basic-tools` and `brainprint-i2` lines for the same scenario
//!   (all of which likewise exclude their own setup).
//! - `brainprint-i3-cold` -- index plus query, the honest cold-start
//!   total.
//!
//! ## How the numbers are obtained
//!
//! - `tool_calls` are the Agent-facing calls the flow makes.
//! - `source_read_bytes`/`source_read_lines` are measured from the
//!   filesystem over exactly the files the flow read: every file for
//!   the indexing line, and the files the preparer verified and sliced
//!   for the query line. Counting them afterwards is measurement
//!   overhead and is not itself counted as a read.
//! - `duplicate_read_bytes` is a re-read of a file already read in the
//!   same flow.
//! - `process_cpu_ms` and `peak_rss_bytes` are `null`, as they are on
//!   the existing `brainprint-i2` lines. The Workspace denies
//!   `unsafe_code`, so `getrusage` -- what the I0 Python baseline uses
//!   for exactly these two columns -- is not callable from here, and a
//!   guess would be worse than a recorded `null`. Wrap the command in
//!   `/usr/bin/time -l` (macOS) or `/usr/bin/time -v` (GNU) to get the
//!   process-level figures; see `benchmarks/README.md`.
//! - The deterministic Agent-token proxies the schema has no column for
//!   (prepared bytes, relation queries, traversal size, confirmed/gap
//!   counts, broad searches avoided) are recorded in `notes` as
//!   `key=value` pairs, which is how the existing lines already carry
//!   scenario detail.
//!
//! No timing claim is made from one run.
//!
//! Run:
//! ```text
//! cargo run -p brainprint-engine --example i3_benchmark -- \
//!   --scenario benchmarks/scenarios/python-signature-impact.json \
//!   --output benchmarks/reports/i3.jsonl
//! ```

use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
    process,
    time::Instant,
};

use brainprint_engine::{
    config::WorkspaceConfig,
    graph::{GraphEndpoint, RelationKind},
    impact::{Budget, ImpactIntent, ImpactTraversal},
    prepare::{InspectPreparer, PreparedInspection},
    query::{QueryIndex, SymbolQuery, SymbolSelector},
    related_tests::{ProjectionOutcome, RelatedTests},
    relations::Direction,
    resource::ResourceStore,
    scan::BaselineScan,
    telemetry::{BenchmarkMetrics, BenchmarkRecorder, BenchmarkResult},
};
use serde_json::Value;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut scenario_path = None;
    let mut output_path = None;
    let mut args = env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--scenario" => scenario_path = args.next(),
            "--output" => output_path = args.next(),
            other => return Err(format!("unexpected argument {other:?}").into()),
        }
    }
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let repo_root = repo_root.canonicalize()?;
    let scenario_path = repo_root.join(scenario_path.ok_or("--scenario is required")?);
    let output_path = repo_root.join(output_path.ok_or("--output is required")?);

    let scenario: Value = serde_json::from_slice(&fs::read(&scenario_path)?)?;
    let scenario_id = scenario["id"].as_str().ok_or("scenario id")?.to_owned();
    let query = scenario["query"]
        .as_str()
        .ok_or("scenario query")?
        .to_owned();
    let fixture = repo_root.join(scenario["workspace"].as_str().ok_or("scenario workspace")?);
    // The same acceptance set the I0 baseline is judged on: the files
    // that must be accounted for. I3 must additionally say *what* each
    // of them is.
    let expected: BTreeSet<String> = scenario["expected_matches"]
        .as_array()
        .ok_or("expected matches")?
        .iter()
        .filter_map(|value| value.as_str().map(ToOwned::to_owned))
        .collect();

    let scratch = env::temp_dir().join(format!("brainprint-i3-benchmark-{}", process::id()));
    let _ = fs::remove_dir_all(&scratch);
    let workspace_root = scratch.join("workspace");
    copy_tree(&fixture, &workspace_root)?;
    let db_path = scratch.join("data").join("index.db");
    let config = WorkspaceConfig::default();

    // --- The cost I3 adds: one baseline scan over the Workspace.
    let index_started = Instant::now();
    let index_cpu_before = cpu_ms();
    BaselineScan::open(&db_path)?.run_initial_scan(&workspace_root, &config, "benchmark-rev-1")?;
    let index_elapsed_ms = elapsed_ms(index_started);
    let index_cpu_ms = delta(index_cpu_before, cpu_ms());
    let (index_bytes, index_lines) = read_totals(&every_file(&workspace_root)?)?;
    let index_rss = peak_rss_bytes();

    // --- The query flow. Three Agent-facing calls, no text search.
    let started = Instant::now();
    let cpu_before = cpu_ms();

    // Call 1: locate the Symbol by name.
    let index = QueryIndex::open(&db_path)?;
    let located = index.search_symbols(&SymbolQuery::new(SymbolSelector::Name(&query)))?;
    let definition = located
        .candidates
        .iter()
        .find(|candidate| candidate.path_rel.ends_with("profile.py"))
        .ok_or("the definition is not in the structural index")?;
    let definition_path = definition.path_rel.clone();
    let target = GraphEndpoint::Symbol(definition.symbol.id);

    // Call 2: one prepared inspection -- confirmed callers, their exact
    // evidence spans, the declarations they sit in, and the
    // definition's own current source.
    let preparer = InspectPreparer::open(&db_path, &workspace_root)?;
    let prepared = preparer.prepare(&target, Direction::Incoming, &[RelationKind::Calls])?;

    // Call 3: related tests, projected from confirmed relation paths.
    let budget = Budget::default();
    let related = RelatedTests::open(&db_path)?.for_target(
        &target,
        ImpactIntent::PublicSignatureChange,
        &budget,
    )?;

    let tool_calls = 3_u64;
    let elapsed = elapsed_ms(started);
    let cpu = delta(cpu_before, cpu_ms());
    let rss = peak_rss_bytes();

    // --- What the flow actually read from disk: the files the
    //     preparer verified in order to slice current source.
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

    // --- What the Agent would actually consume.
    let prepared_bytes: u64 = prepared
        .ranges
        .iter()
        .map(|range| range.source.len() as u64)
        .sum();

    // --- Correctness gate. Performance claims are void without it.
    let resources = ResourceStore::open(&db_path)?;
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
    let test_paths: BTreeSet<String> = related
        .candidates
        .iter()
        .map(|candidate| candidate.path_rel.clone())
        .collect();
    let missing: Vec<&String> = expected.difference(&accounted).collect();
    let definition_source = prepared
        .target
        .as_ref()
        .and_then(|target| target.range)
        .and_then(|id| prepared.range(id))
        .map(|range| range.source.clone())
        .unwrap_or_default();
    let success = missing.is_empty()
        && callers.len() == 3
        && test_paths.len() == 1
        && related.outcome() == ProjectionOutcome::Candidates
        && definition_source.starts_with(&format!("def {query}("))
        && prepared.source_complete()
        && all_confirmed(&prepared);

    // --- Deterministic proxies the schema has no column for.
    let impact = ImpactTraversal::open(&db_path)?.run(
        ImpactIntent::PublicSignatureChange,
        &target,
        &budget,
    )?;
    let notes_query = format!(
        "definition={definition_path}; callers={}; related_test={}; \
         relation_queries=1; prepared_ranges={}; prepared_source_bytes={prepared_bytes}; \
         confirmed_relations={}; gaps={}; traversal_nodes={}; traversal_edges={}; \
         files_read={}; broad_searches=0; repeated_reads=0; coverage={:?}; missing={}",
        join(&callers),
        join(&test_paths),
        prepared.ranges.len(),
        prepared.confirmed_count(),
        prepared.gaps.len(),
        impact.nodes.len(),
        impact.edges.len(),
        read_files.len(),
        related.outcome(),
        if missing.is_empty() {
            "none".to_owned()
        } else {
            missing
                .iter()
                .map(|path| path.as_str())
                .collect::<Vec<_>>()
                .join(",")
        }
    );
    let notes_index = format!(
        "baseline scan over the whole Workspace; files_scanned={}; \
         this is the cost I3 adds before any query",
        every_file(&workspace_root)?.len()
    );
    let notes_cold = format!(
        "index+query cold start; index_elapsed_ms={index_elapsed_ms}; query_elapsed_ms={elapsed}"
    );

    let results = vec![
        result(
            &scenario_id,
            "brainprint-i3-index",
            index_elapsed_ms,
            BenchmarkMetrics {
                tool_calls: Some(1),
                source_read_bytes: Some(index_bytes),
                source_read_lines: Some(index_lines),
                duplicate_read_bytes: Some(0),
                process_cpu_ms: index_cpu_ms,
                peak_rss_bytes: index_rss,
            },
            true,
            &notes_index,
        ),
        result(
            &scenario_id,
            "brainprint-i3",
            elapsed,
            BenchmarkMetrics {
                tool_calls: Some(tool_calls),
                source_read_bytes: Some(read_bytes),
                source_read_lines: Some(read_lines),
                duplicate_read_bytes: Some(0),
                process_cpu_ms: cpu,
                peak_rss_bytes: rss,
            },
            success,
            &notes_query,
        ),
        result(
            &scenario_id,
            "brainprint-i3-cold",
            index_elapsed_ms + elapsed,
            BenchmarkMetrics {
                tool_calls: Some(1 + tool_calls),
                source_read_bytes: Some(index_bytes + read_bytes),
                source_read_lines: Some(index_lines + read_lines),
                // The query re-reads files the scan already read.
                duplicate_read_bytes: Some(read_bytes),
                process_cpu_ms: match (index_cpu_ms, cpu) {
                    (Some(left), Some(right)) => Some(left + right),
                    _ => None,
                },
                peak_rss_bytes: rss,
            },
            success,
            &notes_cold,
        ),
    ];

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut appended = String::new();
    for result in &results {
        appended.push_str(&result.to_json_line()?);
    }
    let mut existing = fs::read_to_string(&output_path).unwrap_or_default();
    existing.push_str(&appended);
    fs::write(&output_path, existing)?;
    let _ = fs::remove_dir_all(&scratch);

    for result in &results {
        println!(
            "{} success={} tool_calls={:?} source_read_bytes={:?} elapsed_ms={} cpu_ms={:?} \
             peak_rss_bytes={:?}",
            result.variant,
            result.success,
            result.metrics.tool_calls,
            result.metrics.source_read_bytes,
            result.elapsed_ms,
            result.metrics.process_cpu_ms,
            result.metrics.peak_rss_bytes
        );
    }
    println!("{notes_query}");
    if results.iter().all(|result| result.success) {
        Ok(())
    } else {
        Err("acceptance mismatch".into())
    }
}

/// Whether every prepared relation is a confirmed, current edge.
fn all_confirmed(prepared: &PreparedInspection) -> bool {
    prepared.relations.iter().all(|relation| {
        relation.relation.resolution == brainprint_engine::resolution::Resolution::Resolved
            && relation
                .evidence
                .iter()
                .all(|evidence| evidence.evidence_range.is_some())
    })
}

fn join(paths: &BTreeSet<String>) -> String {
    paths.iter().cloned().collect::<Vec<_>>().join("|")
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

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn delta(before: Option<u64>, after: Option<u64>) -> Option<u64> {
    match (before, after) {
        (Some(before), Some(after)) => Some(after.saturating_sub(before)),
        _ => None,
    }
}

/// Every file the Workspace discovery would enumerate.
fn every_file(root: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
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

fn read_totals(files: &[PathBuf]) -> Result<(u64, u64), Box<dyn std::error::Error>> {
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

/// Not measurable here: the Workspace denies `unsafe_code`, so
/// `getrusage` is out of reach and these two columns stay `null`
/// rather than becoming an estimate. `benchmarks/README.md` records
/// how to obtain them around the process instead.
const fn cpu_ms() -> Option<u64> {
    None
}

const fn peak_rss_bytes() -> Option<u64> {
    None
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), Box<dyn std::error::Error>> {
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
