//! #53: a verification's per-command capture, consumed as each command
//! ends -- raw output straight into the daemon's [`ArtifactStore`], so a
//! batch holds at most one command's raw bytes -- and shaped into the
//! response: compact diagnostics under one response-wide budget, raw
//! artifact references whose handles are still stored.

use std::sync::Arc;

use brainprint_core::protocol::work::{
    CommandCaptureWire, CommandResultWire, DiagnosticFormatWire, DiagnosticPathWire,
    DiagnosticSeverityWire, DiagnosticSummaryWire, DiagnosticWire, OutputStreamWire,
    RawArtifactRefWire, RawAvailabilityWire, RawStreamMetaWire, StreamStatusWire,
    VerificationCaptureWire,
};
use brainprint_engine::{
    diagnostics::{
        Diagnostic, DiagnosticFormat, DiagnosticPath, DiagnosticSummary, Severity, Stream,
    },
    output_capture::{CaptureRequest, CaptureStatus, CommandCapture},
    verification::CaptureConsumer,
};

use crate::artifacts::{ArtifactRef, ArtifactStore, MAX_COMMAND_BYTES, Reservation, StreamMeta};

/// The serialized `DiagnosticWire` bytes one response may carry: half the
/// 1 MiB frame, leaving the rest for results, raw references and the
/// envelope.
pub(super) const MAX_DIAGNOSTIC_WIRE_BYTES: usize = 512 * 1024;

pub(super) fn request(capture: VerificationCaptureWire) -> CaptureRequest {
    CaptureRequest {
        raw: capture.raw,
        diagnostics: capture.diagnostics.map(|format| match format {
            DiagnosticFormatWire::CargoCompilerMessageJson => {
                DiagnosticFormat::CargoCompilerMessageJson
            }
            DiagnosticFormatWire::PathLineColumn => DiagnosticFormat::PathLineColumn,
        }),
    }
}

/// One command's capture as consumed; diagnostics wait for the
/// response-wide budget.
pub(super) enum Kept {
    NotRequested,
    NotRun,
    Captured {
        stream_status: StreamStatusWire,
        diagnostics: Option<DiagnosticSummary>,
        raw: RawAvailabilityWire,
    },
}

/// Artifacts this request stored that no response has named yet. Dropped
/// undelivered (cancel, disconnect, an abandoned run), it removes them:
/// no sensitive artifact is left behind without a handle to it.
pub(super) struct Undelivered {
    store: Arc<ArtifactStore>,
    handles: Vec<String>,
}

impl Undelivered {
    pub(super) fn new(store: Arc<ArtifactStore>) -> Self {
        Self {
            store,
            handles: Vec::new(),
        }
    }

    /// The response is going out: keep the artifacts it names.
    pub(super) fn deliver(mut self) {
        self.handles.clear();
    }
}

impl Drop for Undelivered {
    fn drop(&mut self) {
        for handle in &self.handles {
            self.store.remove(handle);
        }
    }
}

/// The engine's [`CaptureConsumer`]: reserves a command's raw room before
/// it spawns, commits (or drops) its capture before the next one does.
pub(super) struct StoreConsumer<'a> {
    store: &'a ArtifactStore,
    labels: &'a [String],
    pub(super) kept: Vec<Kept>,
    pub(super) undelivered: Undelivered,
    reservation: Option<Reservation<'a>>,
    raw_requested: bool,
}

impl<'a> StoreConsumer<'a> {
    /// `requested[i]`: whether command `i` has a capture request.
    pub(super) fn new(
        store: &'a ArtifactStore,
        undelivered: Undelivered,
        labels: &'a [String],
        requested: &[bool],
    ) -> Self {
        Self {
            store,
            labels,
            kept: requested
                .iter()
                .map(|&requested| {
                    if requested {
                        Kept::NotRun
                    } else {
                        Kept::NotRequested
                    }
                })
                .collect(),
            undelivered,
            reservation: None,
            raw_requested: false,
        }
    }
}

impl CaptureConsumer for StoreConsumer<'_> {
    fn before(&mut self, index: usize, requested: CaptureRequest) -> CaptureRequest {
        self.raw_requested = requested.raw;
        if !requested.raw {
            return requested;
        }
        match self.store.reserve(MAX_COMMAND_BYTES) {
            Ok(reservation) => {
                self.reservation = Some(reservation);
                requested
            }
            Err(error) => {
                // The command still runs; only its raw output goes unkept.
                eprintln!(
                    "brainprintd: verification {}: raw capture off: {error}",
                    self.labels[index]
                );
                CaptureRequest {
                    raw: false,
                    ..requested
                }
            }
        }
    }

    fn after(&mut self, index: usize, capture: Option<CommandCapture>) {
        let reservation = self.reservation.take();
        let Some(capture) = capture else {
            return;
        };
        let raw = if !self.raw_requested {
            RawAvailabilityWire::NotRequested
        } else if let (Some(reservation), Some(stdout), Some(stderr)) =
            (reservation, &capture.stdout, &capture.stderr)
        {
            match reservation.commit(stdout, stderr) {
                Ok(stored) => {
                    self.undelivered.handles.push(stored.handle.clone());
                    RawAvailabilityWire::Available(reference(stored))
                }
                Err(error) => {
                    eprintln!(
                        "brainprintd: verification {}: raw capture not stored: {error}",
                        self.labels[index]
                    );
                    RawAvailabilityWire::Unavailable
                }
            }
        } else {
            RawAvailabilityWire::Unavailable
        };
        self.kept[index] = Kept::Captured {
            stream_status: match capture.status {
                CaptureStatus::Complete => StreamStatusWire::Complete,
                CaptureStatus::Partial => StreamStatusWire::Partial,
            },
            diagnostics: capture.diagnostics,
            raw,
        };
        // The retained streams are freed here, before the next command.
    }
}

fn reference(stored: ArtifactRef) -> RawArtifactRefWire {
    RawArtifactRefWire {
        handle: stored.handle,
        stdout: stream_meta(stored.stdout),
        stderr: stream_meta(stored.stderr),
    }
}

fn stream_meta(meta: StreamMeta) -> RawStreamMetaWire {
    RawStreamMetaWire {
        observed_bytes: meta.observed_bytes,
        head_bytes: meta.head_bytes,
        tail_bytes: meta.tail_bytes,
        omitted_bytes: meta.omitted_bytes,
        truncated: meta.truncated,
        tail_start_offset: meta.tail_start_offset,
    }
}

/// The response's capture results. Diagnostics are chosen by severity,
/// then command order, then each command's own order, while their
/// serialized size fits [`MAX_DIAGNOSTIC_WIRE_BYTES`]; from the first one
/// that does not fit, the rest are counted as `delivery_omitted`.
pub(super) fn shape(kept: Vec<Kept>) -> Vec<CommandCaptureWire> {
    let mut items: Vec<Vec<(Severity, DiagnosticWire, bool)>> = kept
        .iter()
        .map(|kept| match kept {
            Kept::Captured {
                diagnostics: Some(summary),
                ..
            } => summary
                .items
                .iter()
                .map(|item| (item.severity, diagnostic(item), false))
                .collect(),
            _ => Vec::new(),
        })
        .collect();
    let mut order: Vec<(Severity, usize, usize)> = items
        .iter()
        .enumerate()
        .flat_map(|(command, items)| {
            items
                .iter()
                .enumerate()
                .map(move |(index, item)| (item.0, command, index))
        })
        .collect();
    order.sort_unstable();
    let mut used = 0;
    for (_, command, index) in order {
        let item = &mut items[command][index];
        let size = serde_json::to_vec(&item.1).map_or(usize::MAX, |bytes| bytes.len());
        if size > MAX_DIAGNOSTIC_WIRE_BYTES - used {
            break;
        }
        used += size;
        item.2 = true;
    }

    kept.into_iter()
        .zip(items)
        .map(|(kept, items)| match kept {
            Kept::NotRequested => CommandCaptureWire::NotRequested,
            Kept::NotRun => CommandCaptureWire::NotRun,
            Kept::Captured {
                stream_status,
                diagnostics,
                raw,
            } => CommandCaptureWire::Captured {
                stream_status,
                diagnostics: diagnostics.map(|summary| {
                    let delivered = items.iter().filter(|item| item.2).count();
                    DiagnosticSummaryWire {
                        delivery_omitted: (items.len() - delivered) as u64,
                        items: items
                            .into_iter()
                            .filter_map(|(_, item, chosen)| chosen.then_some(item))
                            .collect(),
                        observed: summary.observed,
                        deduplicated: summary.deduplicated,
                        omitted: summary.omitted,
                        parse_misses: summary.parse_misses,
                    }
                }),
                raw,
            },
        })
        .collect()
}

/// Same-batch eviction: a handle the store no longer holds is not sent as
/// available. Ephemeral all the same: it may go right after the response.
pub(super) fn downgrade_evicted(results: &mut [CommandResultWire], store: &ArtifactStore) {
    for result in results {
        if let CommandCaptureWire::Captured { raw, .. } = &mut result.capture
            && let RawAvailabilityWire::Available(reference) = raw
            && !store.contains(&reference.handle)
        {
            *raw = RawAvailabilityWire::Unavailable;
        }
    }
}

fn diagnostic(item: &Diagnostic) -> DiagnosticWire {
    DiagnosticWire {
        severity: match item.severity {
            Severity::Error => DiagnosticSeverityWire::Error,
            Severity::Warning => DiagnosticSeverityWire::Warning,
            Severity::Note => DiagnosticSeverityWire::Note,
            Severity::Help => DiagnosticSeverityWire::Help,
            Severity::Unknown => DiagnosticSeverityWire::Unknown,
        },
        code: item.code.clone(),
        message: item.message.clone(),
        path: match &item.path {
            DiagnosticPath::Absent => DiagnosticPathWire::Absent,
            DiagnosticPath::Workspace(path) => DiagnosticPathWire::Workspace { path: path.clone() },
            DiagnosticPath::External => DiagnosticPathWire::External,
            DiagnosticPath::Unresolved => DiagnosticPathWire::Unresolved,
        },
        line: item.line,
        column: item.column,
        stream: match item.stream {
            Stream::Stdout => OutputStreamWire::Stdout,
            Stream::Stderr => OutputStreamWire::Stderr,
        },
        message_truncated: item.message_truncated,
    }
}

#[cfg(test)]
mod tests;
