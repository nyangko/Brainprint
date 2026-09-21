//! Minimal benchmark and runtime telemetry primitives.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use brainprint_core::BuildInfo;
use serde::{Deserialize, Serialize};

/// Version of the persisted benchmark result schema.
pub const BENCHMARK_SCHEMA_VERSION: u32 = 1;

/// Build identity copied into a Brainprint-backed benchmark result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchmarkBuild {
    pub product: String,
    pub version: String,
    pub protocol_version: u32,
}

impl From<BuildInfo> for BenchmarkBuild {
    fn from(value: BuildInfo) -> Self {
        Self {
            product: value.product.to_owned(),
            version: value.version.to_owned(),
            protocol_version: value.protocol_version,
        }
    }
}

/// Measurements collected during one benchmark run.
///
/// Optional values are serialized as JSON `null` when the runner could not
/// observe that metric. A measured zero must be recorded as `Some(0)`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchmarkMetrics {
    pub tool_calls: Option<u64>,
    pub source_read_bytes: Option<u64>,
    pub source_read_lines: Option<u64>,
    pub duplicate_read_bytes: Option<u64>,
    pub process_cpu_ms: Option<u64>,
    pub peak_rss_bytes: Option<u64>,
}

/// Stable, line-oriented benchmark result written by I0 tooling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchmarkResult {
    pub schema_version: u32,
    pub run_id: String,
    pub scenario_id: String,
    pub variant: String,
    /// `None` for non-Brainprint baselines.
    pub build: Option<BenchmarkBuild>,
    pub started_at_unix_ms: Option<u64>,
    pub ended_at_unix_ms: Option<u64>,
    pub elapsed_ms: u64,
    pub metrics: BenchmarkMetrics,
    pub success: bool,
    pub failure_kind: Option<String>,
    pub notes: Option<String>,
}

impl BenchmarkResult {
    /// Serialize one result as a JSONL record.
    pub fn to_json_line(&self) -> Result<String, serde_json::Error> {
        let mut line = serde_json::to_string(self)?;
        line.push('\n');
        Ok(line)
    }
}

/// Mutable recorder for one Brainprint-backed benchmark run.
#[derive(Debug)]
pub struct BenchmarkRecorder {
    run_id: String,
    scenario_id: String,
    variant: String,
    build: BenchmarkBuild,
    started_at_unix_ms: Option<u64>,
    started: Instant,
    metrics: BenchmarkMetrics,
}

impl BenchmarkRecorder {
    #[must_use]
    pub fn new(
        run_id: impl Into<String>,
        scenario_id: impl Into<String>,
        variant: impl Into<String>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            scenario_id: scenario_id.into(),
            variant: variant.into(),
            build: BuildInfo::current().into(),
            started_at_unix_ms: unix_time_ms(),
            started: Instant::now(),
            metrics: BenchmarkMetrics::default(),
        }
    }

    pub fn set_tool_calls(&mut self, count: u64) {
        self.metrics.tool_calls = Some(count);
    }

    pub fn record_tool_call(&mut self) {
        self.metrics.tool_calls = Some(self.metrics.tool_calls.unwrap_or(0).saturating_add(1));
    }

    pub fn set_source_reads(&mut self, bytes: u64, lines: u64, duplicate_bytes: u64) {
        self.metrics.source_read_bytes = Some(bytes);
        self.metrics.source_read_lines = Some(lines);
        self.metrics.duplicate_read_bytes = Some(duplicate_bytes);
    }

    pub fn record_source_read(&mut self, bytes: u64, lines: u64, duplicate_bytes: u64) {
        self.metrics.source_read_bytes = Some(
            self.metrics
                .source_read_bytes
                .unwrap_or(0)
                .saturating_add(bytes),
        );
        self.metrics.source_read_lines = Some(
            self.metrics
                .source_read_lines
                .unwrap_or(0)
                .saturating_add(lines),
        );
        self.metrics.duplicate_read_bytes = Some(
            self.metrics
                .duplicate_read_bytes
                .unwrap_or(0)
                .saturating_add(duplicate_bytes),
        );
    }

    pub fn set_process_usage(&mut self, cpu_ms: Option<u64>, peak_rss_bytes: Option<u64>) {
        self.metrics.process_cpu_ms = cpu_ms;
        self.metrics.peak_rss_bytes = peak_rss_bytes;
    }

    #[must_use]
    pub fn finish(
        self,
        success: bool,
        failure_kind: Option<String>,
        notes: Option<String>,
    ) -> BenchmarkResult {
        BenchmarkResult {
            schema_version: BENCHMARK_SCHEMA_VERSION,
            run_id: self.run_id,
            scenario_id: self.scenario_id,
            variant: self.variant,
            build: Some(self.build),
            started_at_unix_ms: self.started_at_unix_ms,
            ended_at_unix_ms: unix_time_ms(),
            elapsed_ms: duration_ms(self.started.elapsed()),
            metrics: self.metrics,
            success,
            failure_kind,
            notes,
        }
    }
}

fn unix_time_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(duration_ms)
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unobserved_metrics_remain_unknown() {
        let result =
            BenchmarkRecorder::new("run-1", "scenario-1", "brainprint").finish(true, None, None);

        assert!(result.build.is_some());
        assert_eq!(result.metrics.tool_calls, None);
        assert_eq!(result.metrics.process_cpu_ms, None);

        let json = result
            .to_json_line()
            .expect("benchmark result should serialize");
        assert!(json.contains("\"tool_calls\":null"));
        assert!(json.contains("\"process_cpu_ms\":null"));
    }

    #[test]
    fn recorder_accumulates_observed_counts() {
        let mut recorder = BenchmarkRecorder::new("run-2", "scenario-1", "brainprint");
        recorder.set_tool_calls(0);
        recorder.record_tool_call();
        recorder.record_source_read(100, 5, 20);
        recorder.record_source_read(50, 2, 10);
        recorder.set_process_usage(Some(12), Some(4096));

        let result = recorder.finish(true, None, None);

        assert_eq!(result.metrics.tool_calls, Some(1));
        assert_eq!(result.metrics.source_read_bytes, Some(150));
        assert_eq!(result.metrics.source_read_lines, Some(7));
        assert_eq!(result.metrics.duplicate_read_bytes, Some(30));
        assert_eq!(result.metrics.process_cpu_ms, Some(12));
        assert_eq!(result.metrics.peak_rss_bytes, Some(4096));
    }
}
