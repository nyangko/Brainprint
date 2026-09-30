//! #52: a Work Result's `verification` — the caller's commands run before
//! the unchanged #50 write, in this order: input checks, `prepare`, the
//! read-only transition preflight, a concurrency slot, the run, the
//! optional #51 `Observe`, then `Job::Work`.
//!
//! Everything that can refuse the request is checked before a command
//! runs. The run is on the blocking pool, never on the Workspace worker.
//! It is cancelled (process tree ended, nothing stored) when the client's
//! connection closes or the daemon shuts down: dropping the waiting
//! future fires the cancel. Nothing is queued or retried.

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
        CommandResultWire, NotStartedReasonWire, VerificationOutcomeWire, VerificationWire,
        WorkErrorWire, WorkFailureWire, WorkOperationWire, WorkOutcomeWire, WorkResponse,
        WorkResultInputWire,
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

use super::{DaemonQueryRuntime, observe, work};

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
    let task = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        prepared.run(&cancel)
    });
    let report = tokio::select! {
        joined = task => joined,
        () = closed => return None,
    };
    let report = match report {
        Ok(Ok(report)) => report,
        Ok(Err(verification::Cancelled)) => return None,
        Err(_) => return Some(failed(work::internal_message("verification task panicked"))),
    };
    let results: Vec<CommandResultWire> = report.results.into_iter().map(result_wire).collect();

    input.verification_summary = Some(report.summary);
    let response =
        match observe::resolve(runtime, workspace, WorkOperationWire::Result(input)).await {
            Ok(operation) => runtime.work(workspace, operation).await,
            Err(failure) => WorkResponse::Failed(failure),
        };
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
    let commands: Vec<VerificationCommand> = verification
        .commands
        .into_iter()
        .map(|command| VerificationCommand {
            label: command.label,
            argv: command.argv,
            cwd: command.cwd,
            env: command.env,
            timeout_secs: command.timeout_secs,
        })
        .collect();
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

const fn failed(error: WorkErrorWire) -> WorkResponse {
    WorkResponse::Failed(work::failure(error))
}

/// Label and outcome only reach the daemon log; never argv, env or output.
fn result_wire(result: CommandResult) -> CommandResultWire {
    eprintln!(
        "brainprintd: verification {}: {:?}",
        result.label, result.outcome
    );
    CommandResultWire {
        label: result.label,
        outcome: match result.outcome {
            VerificationOutcome::Passed => VerificationOutcomeWire::Passed,
            VerificationOutcome::Failed { exit_code } => {
                VerificationOutcomeWire::Failed { exit_code }
            }
            VerificationOutcome::Signaled { signal } => {
                VerificationOutcomeWire::Signaled { signal }
            }
            VerificationOutcome::TimedOut => VerificationOutcomeWire::TimedOut,
            VerificationOutcome::NotStarted { reason } => VerificationOutcomeWire::NotStarted {
                reason: match reason {
                    NotStartedReason::NotFound => NotStartedReasonWire::NotFound,
                    NotStartedReason::PermissionDenied => NotStartedReasonWire::PermissionDenied,
                    NotStartedReason::Other => NotStartedReasonWire::Other,
                },
            },
            VerificationOutcome::Skipped => VerificationOutcomeWire::Skipped,
        },
        duration_ms: result.duration_ms,
        stdout_bytes: result.stdout_bytes,
        stderr_bytes: result.stderr_bytes,
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
