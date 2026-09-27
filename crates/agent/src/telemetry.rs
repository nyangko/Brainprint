//! Optional adoption telemetry (#26 "Adoption telemetry"): one JSONL
//! line per adoption-relevant event, appended to the file named by
//! `BRAINPRINT_ADOPTION_TELEMETRY_PATH`. Disabled when unset. No DB, no
//! table, and never a prompt/source/transcript/tool-result body -- every
//! field below is a closed label, a count, or a digest.

use std::{
    fs::OpenOptions,
    io::{self, Write as _},
    path::PathBuf,
};

use serde::Serialize;

use crate::{
    event::{DecisionKind, EventKind, FallbackReason, IntegrationEvent, Mode, ResetSource},
    gateway::Outcome,
    probe::ProbeStats,
    util,
};

pub const TELEMETRY_ENV: &str = "BRAINPRINT_ADOPTION_TELEMETRY_PATH";

#[derive(Debug, Serialize)]
pub struct TelemetryEvent<'a> {
    pub timestamp_unix_ms: u64,
    pub client: &'a str,
    pub client_version: Option<&'a str>,
    /// Digest of the session id: correlation without the raw id.
    pub session: String,
    pub reset_source: Option<ResetSource>,
    pub mode: Mode,
    pub event: EventKind,
    pub native_attempt: Option<&'static str>,
    pub route: Option<&'static str>,
    pub exact_substitute_proven: bool,
    pub decision: DecisionKind,
    pub fallback_reason: Option<FallbackReason>,
    pub suggested: Option<&'static str>,
    pub latency_us: u64,
    pub daemon_probe_count: u32,
    pub daemon_probe_us: u64,
    pub state_bytes: usize,
    pub bootstrap_bytes: usize,
    pub message_bytes: usize,
}

impl<'a> TelemetryEvent<'a> {
    pub fn new(
        event: &'a IntegrationEvent,
        mode: Mode,
        outcome: &Outcome,
        probes: ProbeStats,
        latency_us: u64,
    ) -> Self {
        Self {
            timestamp_unix_ms: util::now_unix_ms(),
            client: &event.client_id,
            client_version: event.client_version.as_deref(),
            session: util::fingerprint([event.session_id.as_str()])[..16].to_owned(),
            reset_source: event.reset_source,
            mode,
            event: event.kind,
            native_attempt: outcome.native_kind,
            route: outcome.route,
            exact_substitute_proven: outcome.exact_substitute_proven,
            decision: outcome.decision.kind,
            fallback_reason: outcome.decision.fallback,
            suggested: outcome.suggested,
            latency_us,
            daemon_probe_count: probes.count,
            daemon_probe_us: probes.total_micros,
            state_bytes: outcome.state_bytes,
            bootstrap_bytes: outcome.decision.bootstrap.map_or(0, str::len),
            message_bytes: outcome.decision.message.as_deref().map_or(0, str::len),
        }
    }
}

/// `None` = telemetry disabled (the default).
pub fn sink_path() -> Option<PathBuf> {
    std::env::var_os(TELEMETRY_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Append one line; returns the bytes written.
pub fn append(path: &PathBuf, event: &TelemetryEvent<'_>) -> io::Result<usize> {
    let mut line = serde_json::to_vec(event).map_err(io::Error::other)?;
    line.push(b'\n');
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(&line)?;
    Ok(line.len())
}
