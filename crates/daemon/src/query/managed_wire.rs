//! #54 step 3: `Request::VerificationJob*` onto the step 2 managed API --
//! the Workspace resolved as a query does, then one `managed_*` call and
//! a plain conversion of its result. No Job lifecycle of its own.

use brainprint_core::protocol::{
    Response,
    framing::MAX_MESSAGE_BYTES,
    verification_job::{
        CaptureProgressWire, CommandFinishedWire, DiagnosticCountsWire, JobEndReasonWire,
        MAX_POLL_EVENTS, VerificationJobCancelRequestWire, VerificationJobCancelledWire,
        VerificationJobErrorWire, VerificationJobEventPayloadWire, VerificationJobEventWire,
        VerificationJobPollRequestWire, VerificationJobPollWire, VerificationJobStartRequestWire,
        VerificationJobStartedWire, VerificationJobStateWire,
    },
};
use brainprint_engine::verification_job::{MAX_EVENTS_PER_READ, VerificationJobState};

use super::{
    DaemonQueryRuntime,
    handler::resolve,
    managed_verification::{
        CaptureProgress, EndReason, ManagedError, ManagedEvent, ManagedEventPayload,
    },
};

const _: () = assert!(MAX_POLL_EVENTS as usize == MAX_EVENTS_PER_READ);

pub async fn handle_verification_job_start(
    runtime: &DaemonQueryRuntime,
    request: VerificationJobStartRequestWire,
) -> Response {
    let started = async {
        let workspace = resolve(runtime, request.workspace)
            .await
            .map_err(ManagedError::Workspace)?;
        runtime
            .managed_start(workspace, &request.idempotency_key, request.verification)
            .await
    }
    .await;
    Response::VerificationJobStart(
        started
            .map(|start| VerificationJobStartedWire {
                job_id: start.job_id,
                state: state(start.state),
                replayed: start.replayed,
                last_seq: start.last_seq,
            })
            .map_err(error),
    )
}

pub async fn handle_verification_job_poll(
    runtime: &DaemonQueryRuntime,
    request: VerificationJobPollRequestWire,
) -> Response {
    let polled = async {
        let workspace = resolve(runtime, request.workspace)
            .await
            .map_err(ManagedError::Workspace)?;
        runtime
            .managed_poll(
                workspace,
                request.job_id,
                request.after_seq,
                request.limit as usize,
            )
            .await
    }
    .await;
    Response::VerificationJobPoll(polled.map_err(error).and_then(|poll| {
        let mut page = VerificationJobPollWire {
            job_id: poll.job.uid,
            state: state(poll.job.state),
            events: Vec::new(),
            next_seq: poll.next_seq,
            has_more: poll.has_more,
        };
        let events: Vec<_> = poll.events.into_iter().map(event).collect();
        fit_frame(&mut page, events, request.after_seq)?;
        Ok(page)
    }))
}

pub async fn handle_verification_job_cancel(
    runtime: &DaemonQueryRuntime,
    request: VerificationJobCancelRequestWire,
) -> Response {
    let cancelled = async {
        let workspace = resolve(runtime, request.workspace)
            .await
            .map_err(ManagedError::Workspace)?;
        runtime.managed_cancel(workspace, request.job_id).await
    }
    .await;
    Response::VerificationJobCancel(
        cancelled
            .map(|ended| VerificationJobCancelledWire {
                job_id: request.job_id,
                state: state(ended),
            })
            .map_err(error),
    )
}

/// Moves `events` (ascending) into `page` while the whole response still
/// encodes within one frame; the first that would not fit and every one
/// after it are left for the next poll (`has_more`). A terminal payload
/// is bounded below the frame (step 2), so the first event always fits.
fn fit_frame(
    page: &mut VerificationJobPollWire,
    events: Vec<VerificationJobEventWire>,
    after_seq: u64,
) -> Result<(), VerificationJobErrorWire> {
    fn encoded(value: &impl serde::Serialize) -> serde_json::Result<usize> {
        serde_json::to_vec(value).map(|json| json.len())
    }
    let internal = || VerificationJobErrorWire::Internal {
        message: "verification job poll could not be encoded".to_owned(),
    };
    // The envelope at its widest: the cursor at most 20 digits, `false`
    // one byte longer than `true`.
    let (next_seq, has_more) = (page.next_seq, page.has_more);
    page.next_seq = u64::MAX;
    page.has_more = false;
    let mut size =
        encoded(&Response::VerificationJobPoll(Ok(page.clone()))).map_err(|_| internal())?;
    let total = events.len();
    for event in events {
        // `,` between events.
        let added = encoded(&event).map_err(|_| internal())? + usize::from(!page.events.is_empty());
        if size + added > MAX_MESSAGE_BYTES as usize {
            break;
        }
        size += added;
        page.events.push(event);
    }
    if page.events.is_empty() && total > 0 {
        eprintln!("brainprintd: managed verification: an event exceeds the frame");
        return Err(internal());
    }
    if page.events.len() < total {
        page.next_seq = page.events.last().map_or(after_seq, |event| event.seq);
        page.has_more = true;
    } else {
        page.next_seq = next_seq;
        page.has_more = has_more;
    }
    Ok(())
}

fn state(state: VerificationJobState) -> VerificationJobStateWire {
    match state {
        VerificationJobState::Running => VerificationJobStateWire::Running,
        VerificationJobState::Finished => VerificationJobStateWire::Finished,
        VerificationJobState::Cancelled => VerificationJobStateWire::Cancelled,
        VerificationJobState::Interrupted => VerificationJobStateWire::Interrupted,
        VerificationJobState::InternalError => VerificationJobStateWire::InternalError,
    }
}

fn error(error: ManagedError) -> VerificationJobErrorWire {
    match error {
        ManagedError::Workspace(error) => VerificationJobErrorWire::Workspace(error),
        ManagedError::InvalidVerification(reason) => {
            VerificationJobErrorWire::InvalidVerification { reason }
        }
        ManagedError::InvalidIdempotencyKey => VerificationJobErrorWire::InvalidIdempotencyKey,
        ManagedError::IdempotencyConflict => VerificationJobErrorWire::IdempotencyConflict,
        ManagedError::VerificationBusy => VerificationJobErrorWire::VerificationBusy,
        ManagedError::JobNotFound => VerificationJobErrorWire::JobNotFound,
        ManagedError::InvalidLimit => VerificationJobErrorWire::InvalidLimit,
        ManagedError::Corrupt => VerificationJobErrorWire::Corrupt,
        ManagedError::Internal(message) => VerificationJobErrorWire::Internal {
            message: message.to_owned(),
        },
    }
}

fn event(event: ManagedEvent) -> VerificationJobEventWire {
    use ManagedEventPayload as P;
    use VerificationJobEventPayloadWire as W;
    VerificationJobEventWire {
        seq: event.seq,
        created_at: event.created_at,
        payload: match event.payload {
            P::JobStarted(_) => W::JobStarted,
            P::CommandStarted(started) => W::CommandStarted {
                index: started.index,
                label: started.label,
            },
            P::CommandFinished(finished) => W::CommandFinished(CommandFinishedWire {
                index: finished.index,
                label: finished.label,
                outcome: finished.outcome,
                duration_ms: finished.duration_ms,
                stdout_bytes: finished.stdout_bytes,
                stderr_bytes: finished.stderr_bytes,
                capture: match finished.capture {
                    CaptureProgress::NotRequested => CaptureProgressWire::NotRequested,
                    CaptureProgress::NotRun => CaptureProgressWire::NotRun,
                    CaptureProgress::Captured {
                        stream_status,
                        diagnostics,
                        raw,
                    } => CaptureProgressWire::Captured {
                        stream_status,
                        diagnostics: diagnostics.map(|counts| DiagnosticCountsWire {
                            observed: counts.observed,
                            deduplicated: counts.deduplicated,
                            omitted: counts.omitted,
                            parse_misses: counts.parse_misses,
                            retained: counts.retained,
                        }),
                        raw,
                    },
                },
            }),
            P::JobFinished(finished) => W::JobFinished {
                verification_summary: finished.verification_summary,
                results: finished.results,
            },
            P::JobCancelled(ended) => W::JobCancelled {
                reason: reason(ended.reason),
            },
            P::JobInterrupted(ended) => W::JobInterrupted {
                reason: reason(ended.reason),
            },
            P::JobInternalError(ended) => W::JobInternalError {
                reason: reason(ended.reason),
            },
        },
    }
}

fn reason(reason: EndReason) -> JobEndReasonWire {
    match reason {
        EndReason::CallerCancelled => JobEndReasonWire::CallerCancelled,
        EndReason::DaemonShutdown => JobEndReasonWire::DaemonShutdown,
        EndReason::DaemonRestart => JobEndReasonWire::DaemonRestart,
        EndReason::EventPersistence => JobEndReasonWire::EventPersistence,
        EndReason::EventPayload => JobEndReasonWire::EventPayload,
        EndReason::RunnerFailure => JobEndReasonWire::RunnerFailure,
    }
}

#[cfg(test)]
mod tests {
    use brainprint_core::VerificationJobId;

    use super::*;

    fn page() -> VerificationJobPollWire {
        VerificationJobPollWire {
            job_id: VerificationJobId::generate(),
            state: VerificationJobStateWire::Finished,
            events: Vec::new(),
            next_seq: 4,
            has_more: false,
        }
    }

    fn big(seq: u64, bytes: usize) -> VerificationJobEventWire {
        VerificationJobEventWire {
            seq,
            created_at: "2026-10-01T00:00:00Z".to_owned(),
            payload: VerificationJobEventPayloadWire::JobFinished {
                verification_summary: "s".repeat(bytes),
                results: Vec::new(),
            },
        }
    }

    fn frame(page: &VerificationJobPollWire) -> usize {
        serde_json::to_vec(&Response::VerificationJobPoll(Ok(page.clone())))
            .expect("encode")
            .len()
    }

    /// Events that together pass the frame are split in seq order; the
    /// next page starts exactly after the last one sent.
    #[test]
    fn a_page_stops_before_the_event_that_would_pass_the_frame() {
        let events: Vec<_> = (1..=4).map(|seq| big(seq, 400 * 1024)).collect();
        let mut first = page();
        fit_frame(&mut first, events[..].to_vec(), 0).expect("fits");
        let seqs: Vec<u64> = first.events.iter().map(|event| event.seq).collect();
        assert_eq!(seqs, [1, 2]);
        assert_eq!((first.next_seq, first.has_more), (2, true));
        assert!(frame(&first) <= MAX_MESSAGE_BYTES as usize);

        let mut second = page();
        fit_frame(&mut second, events[2..].to_vec(), 2).expect("fits");
        let seqs: Vec<u64> = second.events.iter().map(|event| event.seq).collect();
        assert_eq!(seqs, [3, 4]);
        assert_eq!((second.next_seq, second.has_more), (4, false));
    }

    /// The accounting is exact: a page filled to the frame's last byte is
    /// kept whole, one byte more is split.
    #[test]
    fn the_size_accounting_matches_the_encoding() {
        let mut probe = page();
        fit_frame(&mut probe, vec![big(1, 0), big(2, 0)], 0).expect("fits");
        // Measured at the widest cursor, as the accounting does.
        probe.next_seq = u64::MAX;
        let room = MAX_MESSAGE_BYTES as usize - frame(&probe);
        let mut full = page();
        fit_frame(&mut full, vec![big(1, 0), big(2, room)], 0).expect("fits");
        assert_eq!(full.events.len(), 2);
        assert!(frame(&full) <= MAX_MESSAGE_BYTES as usize);
        let mut over = page();
        fit_frame(&mut over, vec![big(1, 0), big(2, room + 1)], 0).expect("fits");
        assert_eq!(
            (over.events.len(), over.next_seq, over.has_more),
            (1, 1, true)
        );
    }

    #[test]
    fn an_empty_delta_keeps_the_cursor() {
        let mut empty = page();
        fit_frame(&mut empty, Vec::new(), 4).expect("fits");
        assert_eq!(
            (empty.events.len(), empty.next_seq, empty.has_more),
            (0, 4, false)
        );
    }
}
