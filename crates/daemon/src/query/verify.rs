//! #52: a Work Result's `verification` — the caller's commands run before
//! the unchanged #50 write, in this order: input checks, `prepare`, the
//! read-only transition preflight, a concurrency slot, the #55
//! pre-command baseline, the run, the #55 post-command refresh, the
//! optional #51 `Observe`, then `Job::Work` held to the refreshed basis.
//! The slot is held from before the baseline until after the write.
//!
//! Everything that can refuse the request is checked before a command
//! runs. The run is on the blocking pool, never on the Workspace worker.
//! It is cancelled (process tree ended, nothing stored, no answer) when
//! the client's connection closes or the daemon shuts down: dropping the
//! waiting future fires the cancel. Baseline, run and refresh belong to a
//! daemon task, not to the request: a cancelled run is still followed by
//! its refresh before the slot is freed. Nothing is queued or retried.
//!
//! #53: when a command asks for capture, the batch runs through
//! [`capture::StoreConsumer`]; otherwise it is the #52 run unchanged.

// #52 grew `WorkFailureWire` (the per-command results) past the lint's
// size; it is built once per Work request, never in a hot loop.
#![allow(clippy::result_large_err)]

use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use brainprint_core::{
    WorkspaceId,
    protocol::work::{
        CommandCaptureWire, CommandResultWire, NotStartedReasonWire, VerificationOutcomeWire,
        VerificationWire, WorkErrorWire, WorkFailureWire, WorkOperationWire, WorkOutcomeWire,
        WorkResponse, WorkResultInputWire,
    },
};
use brainprint_engine::{
    knowledge::ResultOutcome,
    paths::WorkspacePaths,
    process_runner::Cancel,
    verification::{
        self, CommandResult, NotStartedReason, VerificationCommand, VerificationOutcome,
    },
};

use super::{
    DaemonQueryRuntime,
    capture::{self, Kept, StoreConsumer, Undelivered},
    lifecycle::IndexBasis,
    observe,
    runtime::WorkerHandle,
    work::{self, WorkContext},
};

/// Verifications running at once in the whole daemon.
pub(super) const DAEMON_LIMIT: usize = 4;

/// The Workspaces running a verification. One per Workspace, so also one
/// per WorkItem (a WorkItem lives in one Workspace's database); the set's
/// size is the daemon-wide count. One lock, taken all-or-nothing.
#[derive(Debug, Clone, Default)]
pub(super) struct Slots(Arc<SlotState>);

#[derive(Debug, Default)]
struct SlotState {
    running: Mutex<HashSet<WorkspaceId>>,
    /// Batches started since the daemon began (acceptance seam).
    started: AtomicU64,
}

impl Slots {
    /// No waiting: `None` is `VerificationBusy`.
    pub(super) fn try_acquire(&self, workspace: WorkspaceId) -> Option<Slot> {
        let mut running = self
            .0
            .running
            .lock()
            .expect("verification slots mutex poisoned");
        if running.len() >= DAEMON_LIMIT || !running.insert(workspace) {
            return None;
        }
        self.0.started.fetch_add(1, Ordering::Relaxed);
        Some(Slot {
            slots: self.clone(),
            workspace,
        })
    }

    pub(super) fn started(&self) -> u64 {
        self.0.started.load(Ordering::Relaxed)
    }
}

/// Held by the run itself, so a cancelled tree still holds its Workspace
/// until it has ended.
pub(super) struct Slot {
    slots: Slots,
    workspace: WorkspaceId,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.slots
            .0
            .running
            .lock()
            .expect("verification slots mutex poisoned")
            .remove(&self.workspace);
    }
}

struct CancelOnDrop(Cancel);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// #55, protocol 8: no current basis before the commands; none ran.
const BASELINE_FAILED: &str =
    "the workspace could not be proven current before the verification; nothing ran";
/// #55, protocol 8: no current basis after the commands; nothing stored.
const REFRESH_FAILED: &str =
    "the workspace could not be proven current after the verification; nothing was stored";

/// #55: whether a command's outcome means a process was spawned (and so
/// may have changed the Workspace). `NotStarted`/`Skipped` never were.
pub(super) const fn spawned(outcome: VerificationOutcome) -> bool {
    !matches!(
        outcome,
        VerificationOutcome::NotStarted { .. } | VerificationOutcome::Skipped
    )
}

/// A finished batch handed back to the request, with the slot: it is
/// freed after the write, or here if the request is gone.
struct Ran {
    slot: Slot,
    report: verification::VerificationReport,
    kept: Vec<Kept>,
    undelivered: Undelivered,
    /// The post-command basis (the baseline's if nothing was spawned).
    refreshed: Result<IndexBasis, ()>,
}

enum Stopped {
    Baseline,
    Cancelled,
    Panicked,
}

/// `None`: the client left while the commands ran; they were cancelled
/// and nothing was stored or can be answered.
pub(super) async fn run(
    runtime: &DaemonQueryRuntime,
    workspace: WorkspaceId,
    mut input: WorkResultInputWire,
    closed: impl Future<Output = ()>,
) -> Option<WorkResponse> {
    let Some(verification) = input.verification.take() else {
        return Some(failed(work::internal_message("no verification to run")));
    };
    let prepared = match preflight(runtime, workspace, &input, verification).await {
        Ok(prepared) => prepared,
        Err(failure) => return Some(WorkResponse::Failed(failure)),
    };
    let Some(slot) = runtime.verification_slots().try_acquire(workspace) else {
        return Some(failed(WorkErrorWire::VerificationBusy));
    };

    let cancel = Cancel::default();
    let _cancel_on_drop = CancelOnDrop(cancel.clone());
    let store = runtime.artifact_store();
    let worker = runtime.handle_for(workspace);
    let (handback, handed) = tokio::sync::oneshot::channel();
    tokio::spawn(continuation(
        worker.clone(),
        slot,
        prepared,
        cancel,
        Arc::clone(&store),
        handback,
    ));
    // A request that leaves stops waiting; the continuation still ends
    // the tree, refreshes and frees the slot. Its undelivered artifacts
    // are removed when its output is dropped.
    let handed = tokio::select! {
        handed = handed => handed,
        () = closed => return None,
    };
    let Ran {
        slot,
        report,
        kept,
        undelivered,
        refreshed,
    } = match handed {
        Ok(Ok(ran)) => ran,
        Ok(Err(Stopped::Cancelled)) => return None,
        Ok(Err(Stopped::Baseline)) => return Some(failed(work::internal_message(BASELINE_FAILED))),
        Ok(Err(Stopped::Panicked)) | Err(_) => {
            return Some(failed(work::internal_message("verification task panicked")));
        }
    };
    let mut results: Vec<CommandResultWire> = report
        .results
        .into_iter()
        .zip(capture::shape(kept))
        .map(|(result, capture)| result_wire(result, capture))
        .collect();

    let response = match refreshed {
        // A command may have changed the Workspace and its basis is
        // unknown: no result is stored against a stale one.
        Err(()) => failed(work::internal_message(REFRESH_FAILED)),
        Ok(after) => {
            input.verification_summary = Some(report.summary);
            match observe::resolve(runtime, workspace, WorkOperationWire::Result(input)).await {
                Ok(operation) => worker.work(operation, WorkContext::at(&after)).await,
                Err(failure) => WorkResponse::Failed(failure),
            }
        }
    };
    drop(slot);
    capture::downgrade_evicted(&mut results, &store);
    // Recorded or not, the response names the artifacts: they stay.
    undelivered.deliver();
    Some(match response {
        WorkResponse::Recorded(mut recorded) => {
            recorded.verification = Some(results);
            WorkResponse::Recorded(recorded)
        }
        WorkResponse::Failed(mut failure) => {
            failure.verification = Some(results);
            WorkResponse::Failed(failure)
        }
        started @ WorkResponse::Started(_) => started,
    })
}

/// Input checks, `prepare` and the transition preflight: nothing runs
/// unless all pass.
async fn preflight(
    runtime: &DaemonQueryRuntime,
    workspace: WorkspaceId,
    input: &WorkResultInputWire,
    verification: VerificationWire,
) -> Result<verification::PreparedVerification, WorkFailureWire> {
    if input.verification_summary.is_some() {
        return Err(work::failure(WorkErrorWire::InvalidObservation {
            reason: "verification and verification_summary are exclusive".to_owned(),
        }));
    }
    work::check_result_input(input)?;
    let root = runtime
        .workspace_root(workspace)
        .await
        .map_err(|error| work::failure(WorkErrorWire::Workspace(error)))?;
    let work_item = input.work_item;
    let outcome = match input.outcome {
        WorkOutcomeWire::Partial => ResultOutcome::Partial,
        WorkOutcomeWire::Complete => ResultOutcome::Complete,
        WorkOutcomeWire::Abandon => ResultOutcome::Abandon,
    };
    let commands = commands(verification);
    tokio::task::spawn_blocking(move || {
        let prepared = verification::prepare(&root, &commands).map_err(|error| {
            work::failure(WorkErrorWire::InvalidObservation {
                reason: error.to_string(),
            })
        })?;
        work::open(workspace, &WorkspacePaths::from_root(&root))?
            .check_result_allowed(work_item, outcome)
            .map_err(|error| work::failure(work::work_error(error, Some(work_item))))?;
        Ok(prepared)
    })
    .await
    .unwrap_or_else(|_| {
        Err(work::failure(work::internal_message(
            "verification preflight panicked",
        )))
    })
}

/// #55: the part of a synchronous verification the request does not own:
/// baseline, run, then -- whatever became of the request -- the refresh
/// after a run that may have spawned anything. The slot is held across
/// all of it.
async fn continuation(
    worker: WorkerHandle,
    slot: Slot,
    prepared: verification::PreparedVerification,
    cancel: Cancel,
    store: Arc<crate::artifacts::ArtifactStore>,
    handback: tokio::sync::oneshot::Sender<Result<Ran, Stopped>>,
) {
    // The daemon's own shutdown drops this task: the tree still ends.
    let _cancel_on_drop = CancelOnDrop(cancel.clone());
    let baseline = match worker.command_baseline().await {
        Ok(baseline) => baseline,
        Err(error) => {
            eprintln!("brainprintd: verification baseline failed: {error}");
            let _ = handback.send(Err(Stopped::Baseline));
            return;
        }
    };
    if cancel.is_cancelled() {
        // Gone before any command: nothing to refresh.
        let _ = handback.send(Err(Stopped::Cancelled));
        return;
    }
    let batch = {
        let cancel = cancel.clone();
        tokio::task::spawn_blocking(move || run_batch(&prepared, &cancel, store)).await
    };
    // A cancelled or failed run may have spawned anything: refreshed too.
    let may_have_run = match &batch {
        Ok(Ok((report, ..))) => report.results.iter().any(|result| spawned(result.outcome)),
        _ => true,
    };
    let refreshed = if may_have_run {
        worker
            .post_command_refresh(baseline)
            .await
            .map(|report| report.after)
            .map_err(|error| eprintln!("brainprintd: verification refresh failed: {error}"))
    } else {
        Ok(baseline.basis)
    };
    let _ = handback.send(match batch {
        Ok(Ok((report, kept, undelivered))) => Ok(Ran {
            slot,
            report,
            kept,
            undelivered,
            refreshed,
        }),
        Ok(Err(verification::Cancelled)) => Err(Stopped::Cancelled),
        Err(_) => Err(Stopped::Panicked),
    });
}

/// The caller's commands as the engine takes them; shared with the #54
/// managed Jobs.
pub(super) fn commands(verification: VerificationWire) -> Vec<VerificationCommand> {
    verification
        .commands
        .into_iter()
        .map(|command| VerificationCommand {
            label: command.label,
            argv: command.argv,
            cwd: command.cwd,
            env: command.env,
            timeout_secs: command.timeout_secs,
            capture: command.capture.map(capture::request),
        })
        .collect()
}

/// The #52 run when no command asks for capture; otherwise each capture
/// goes to the store as its command ends. `Kept` is per command.
fn run_batch(
    prepared: &verification::PreparedVerification,
    cancel: &Cancel,
    store: Arc<crate::artifacts::ArtifactStore>,
) -> Result<(verification::VerificationReport, Vec<Kept>, Undelivered), verification::Cancelled> {
    let requested = prepared.capture_requested();
    let undelivered = Undelivered::new(Arc::clone(&store));
    if !requested.contains(&true) {
        let report = prepared.run(cancel)?;
        let kept = requested.iter().map(|_| Kept::NotRequested).collect();
        return Ok((report, kept, undelivered));
    }
    let labels = prepared.labels();
    let mut consumer = StoreConsumer::new(&store, undelivered, &labels, &requested);
    let report = prepared.run_capturing(cancel, &mut consumer)?;
    Ok((report, consumer.kept, consumer.undelivered))
}

const fn failed(error: WorkErrorWire) -> WorkResponse {
    WorkResponse::Failed(work::failure(error))
}

/// Label and outcome only reach the daemon log; never argv, env or output.
pub(super) fn result_wire(result: CommandResult, capture: CommandCaptureWire) -> CommandResultWire {
    eprintln!(
        "brainprintd: verification {}: {:?}",
        result.label, result.outcome
    );
    CommandResultWire {
        label: result.label,
        outcome: outcome_wire(result.outcome),
        duration_ms: result.duration_ms,
        stdout_bytes: result.stdout_bytes,
        stderr_bytes: result.stderr_bytes,
        capture,
    }
}

pub(super) const fn outcome_wire(outcome: VerificationOutcome) -> VerificationOutcomeWire {
    match outcome {
        VerificationOutcome::Passed => VerificationOutcomeWire::Passed,
        VerificationOutcome::Failed { exit_code } => VerificationOutcomeWire::Failed { exit_code },
        VerificationOutcome::Signaled { signal } => VerificationOutcomeWire::Signaled { signal },
        VerificationOutcome::TimedOut => VerificationOutcomeWire::TimedOut,
        VerificationOutcome::NotStarted { reason } => VerificationOutcomeWire::NotStarted {
            reason: match reason {
                NotStartedReason::NotFound => NotStartedReasonWire::NotFound,
                NotStartedReason::PermissionDenied => NotStartedReasonWire::PermissionDenied,
                NotStartedReason::Other => NotStartedReasonWire::Other,
            },
        },
        VerificationOutcome::Skipped => VerificationOutcomeWire::Skipped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One per Workspace (so per WorkItem), four daemon-wide, no waiting;
    /// a dropped slot frees its Workspace.
    #[test]
    fn slots_limit_per_workspace_and_daemon_wide() {
        let slots = Slots::default();
        let workspaces: Vec<WorkspaceId> = (1..=5_u8)
            .map(|n| WorkspaceId::from_bytes([n; 16]))
            .collect();
        let first = slots.try_acquire(workspaces[0]).expect("first");
        assert!(slots.try_acquire(workspaces[0]).is_none(), "same Workspace");
        let held: Vec<Slot> = workspaces[1..4]
            .iter()
            .map(|&workspace| slots.try_acquire(workspace).expect("within 4"))
            .collect();
        assert!(slots.try_acquire(workspaces[4]).is_none(), "the fifth");
        drop(first);
        let fifth = slots.try_acquire(workspaces[4]).expect("a slot was freed");
        assert!(slots.try_acquire(workspaces[0]).is_none(), "full again");
        drop((held, fifth));
        assert!(slots.try_acquire(workspaces[0]).is_some(), "all freed");
        assert_eq!(slots.started(), 6);
    }
}
