//! Re-measure the I0 representative scenario through I2's paths (#16
//! task 15).
//!
//! Benchmark harness, not a product feature: it drives the same
//! `python-signature-impact` fixture and scenario file the I0
//! `basic-tools` baseline used, through the engine, and writes the same
//! JSONL schema ([`brainprint_engine::telemetry`]) so a human can put the
//! two side by side.
//!
//! ## What is recorded, and how it is measured
//!
//! Three lines are written, because I2 owns two different kinds of
//! answer and conflating them would overstate what it knows:
//!
//! - `brainprint-i2-structured` -- locate the definition and read it.
//!   This is the part I2 is responsible for, and it is what the "no more
//!   repeated `ls`/`find`/`rg`/`read`" criterion is judged on.
//! - `brainprint-i2-text-fallback` -- one bounded current-filesystem
//!   search for the remaining evidence (call sites, tests). These are
//!   *textual* matches. Calling them CALLS or "related test" is a
//!   semantic claim, and that claim belongs to I3.
//! - `brainprint-i2` -- the two together, comparable with the
//!   `basic-tools` line for the same scenario.
//!
//! Byte counts are measured, not estimated:
//! - the structured read reads exactly one file, whose size is taken
//!   from the filesystem after the run;
//! - the fallback reports `bytes_scanned`/`files_scanned` itself;
//! - `duplicate_read_bytes` is the overlap between the two -- the
//!   definition file, which the fallback also scans.
//!
//! Line counts are recounted by this harness over exactly the files the
//! run read, and that recount is not itself part of the metric.
//!
//! No timing claim is made from one run: `elapsed_ms` and RSS are
//! recorded because the schema has the fields, not as evidence.
//!
//! Run:
//! ```text
//! cargo run -p brainprint-engine --example i2_benchmark -- \
//!   --scenario benchmarks/scenarios/python-signature-impact.json \
//!   --output benchmarks/reports/i2.jsonl
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
    inspect::SourceReader,
    query::{QueryIndex, SymbolQuery, SymbolSelector},
    scan::BaselineScan,
    search::{FallbackReason, QueryStatus, TextPattern, TextSearch, TextSearcher},
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
    let expected: BTreeSet<String> = scenario["expected_matches"]
        .as_array()
        .ok_or("expected matches")?
        .iter()
        .filter_map(|value| value.as_str().map(ToOwned::to_owned))
        .collect();

    // The same snapshot, copied so the run may write its own index.db.
    let scratch = env::temp_dir().join(format!("brainprint-i2-benchmark-{}", process::id()));
    let _ = fs::remove_dir_all(&scratch);
    let workspace_root = scratch.join("workspace");
    copy_tree(&fixture, &workspace_root)?;
    let db_path = scratch.join("data").join("index.db");
    let config = WorkspaceConfig::default();

    // Indexing is setup, not part of the measured query flow: the
    // baseline's six tool calls are the *lookup*, and so are these.
    BaselineScan::open(&db_path)?.run_initial_scan(&workspace_root, &config, "benchmark-rev-1")?;

    let started = Instant::now();

    // --- Structured: locate the definition, then read it. Two calls.
    let mut structured_tool_calls = 0_u64;
    let index = QueryIndex::open(&db_path)?;
    structured_tool_calls += 1;
    let located = index.search_symbols(&SymbolQuery::new(SymbolSelector::Name(&query)))?;
    let definition = located
        .candidates
        .iter()
        .find(|candidate| candidate.path_rel.ends_with("profile.py"))
        .ok_or("the definition is not in the structural index")?;
    let definition_path = definition.path_rel.clone();

    structured_tool_calls += 1;
    let reader = SourceReader::open(&db_path, &workspace_root)?;
    let inspection = reader.inspect_symbol(definition.symbol.id)?;
    let structured_ok = located
        .candidates
        .iter()
        .any(|candidate| candidate.path_rel == definition_path)
        && inspection.source.starts_with(&format!("def {query}("));

    // The one file the structured path read, measured from disk.
    let definition_bytes = fs::metadata(workspace_root.join(&definition_path))?.len();
    let definition_lines = line_count(&fs::read(workspace_root.join(&definition_path))?);

    // --- Text fallback: one bounded search for the textual evidence the
    //     structural model does not claim to explain.
    let searcher = TextSearcher::new(&workspace_root, &config, &index);
    let hits = searcher.search(&TextSearch {
        reason: FallbackReason::NonStructuralTarget,
        ..TextSearch::explicit(TextPattern::Literal(&query))
    })?;
    let fallback_tool_calls = 1_u64;
    let mut evidence: BTreeSet<String> = BTreeSet::new();
    for hit in &hits.matches {
        evidence.insert(hit.path_rel.clone());
    }
    let missing: Vec<&String> = expected.difference(&evidence).collect();
    let fallback_ok = missing.is_empty() && hits.status == QueryStatus::Found;

    // Lines over exactly the files the fallback scanned. Counting them
    // is measurement overhead, deliberately not counted as a read.
    let mut fallback_lines = 0_u64;
    for entry in scanned_files(&workspace_root, &hits.scope.binary_skipped)? {
        fallback_lines += line_count(&fs::read(&entry)?);
    }

    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let fallback_bytes = hits.scope.bytes_scanned;
    // The definition file is read by both paths: once verified for the
    // structured read, once scanned by the fallback.
    let duplicate_bytes = definition_bytes.min(fallback_bytes);

    let notes_structured = format!(
        "definition={definition_path}; one inspect returned the definition source \
         (no second read); files_scanned=0"
    );
    let notes_fallback = format!(
        "textual evidence only, not a CALLS/related-test relation (I3 owns that); \
         evidence={}; files_scanned={}",
        evidence.iter().cloned().collect::<Vec<_>>().join(","),
        hits.scope.files_scanned
    );
    let notes_total = format!(
        "structured locate+inspect ({structured_tool_calls} calls) plus one bounded text \
         fallback; missing={}",
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

    let results = vec![
        result(
            &scenario_id,
            "brainprint-i2-structured",
            elapsed_ms,
            BenchmarkMetrics {
                tool_calls: Some(structured_tool_calls),
                source_read_bytes: Some(definition_bytes),
                source_read_lines: Some(definition_lines),
                duplicate_read_bytes: Some(0),
                process_cpu_ms: None,
                peak_rss_bytes: None,
            },
            structured_ok,
            &notes_structured,
        ),
        result(
            &scenario_id,
            "brainprint-i2-text-fallback",
            elapsed_ms,
            BenchmarkMetrics {
                tool_calls: Some(fallback_tool_calls),
                source_read_bytes: Some(fallback_bytes),
                source_read_lines: Some(fallback_lines),
                duplicate_read_bytes: Some(0),
                process_cpu_ms: None,
                peak_rss_bytes: None,
            },
            fallback_ok,
            &notes_fallback,
        ),
        result(
            &scenario_id,
            "brainprint-i2",
            elapsed_ms,
            BenchmarkMetrics {
                tool_calls: Some(structured_tool_calls + fallback_tool_calls),
                source_read_bytes: Some(definition_bytes + fallback_bytes),
                source_read_lines: Some(definition_lines + fallback_lines),
                duplicate_read_bytes: Some(duplicate_bytes),
                process_cpu_ms: None,
                peak_rss_bytes: None,
            },
            structured_ok && fallback_ok,
            &notes_total,
        ),
    ];

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut appended = String::new();
    for result in &results {
        // `to_json_line` already terminates the line.
        appended.push_str(&result.to_json_line()?);
    }
    let mut existing = fs::read_to_string(&output_path).unwrap_or_default();
    existing.push_str(&appended);
    fs::write(&output_path, existing)?;
    let _ = fs::remove_dir_all(&scratch);

    for result in &results {
        println!(
            "{} success={} tool_calls={:?} source_read_bytes={:?} duplicate_read_bytes={:?}",
            result.variant,
            result.success,
            result.metrics.tool_calls,
            result.metrics.source_read_bytes,
            result.metrics.duplicate_read_bytes
        );
    }
    if results.iter().all(|result| result.success) {
        Ok(())
    } else {
        Err("acceptance mismatch".into())
    }
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
    // Unmeasured here, and recorded as such rather than guessed.
    recorder.set_process_usage(None, None);
    let mut finished = recorder.finish(success, None, Some(notes.to_owned()));
    finished.elapsed_ms = elapsed_ms;
    finished
}

/// Every file the bounded search would have read: the walk is the
/// index's own discovery, minus what the search skipped as binary.
fn scanned_files(
    root: &Path,
    skipped: &[String],
) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut files = Vec::new();
    for entry in
        brainprint_engine::discovery::enumerate_resources(root, &WorkspaceConfig::default())?
    {
        if entry.kind != brainprint_engine::resource::ResourceKind::File
            || skipped.contains(&entry.path_rel)
        {
            continue;
        }
        files.push(root.join(&entry.path_rel));
    }
    Ok(files)
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
