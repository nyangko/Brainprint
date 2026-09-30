//! #51: resolve a Work request's `Observe` into Brainprint's own Git
//! observation before the unchanged #50 write runs.
//!
//! Git runs on the blocking pool, never on the Workspace worker, so the
//! Workspace's queries and index publication are not held up by it. A
//! failed observation returns before `Job::Work`: nothing is written, and
//! `Unknown` is never recorded in its place.

// #52 grew `WorkFailureWire` (the per-command results) past the lint's
// size; it is built once per Work request, never in a hot loop.
#![allow(clippy::result_large_err)]

use brainprint_core::{
    WorkspaceId,
    protocol::work::{
        GitEntryStatusWire, GitEntryWire, GitObservationFailureWire, GitObservationWire,
        WorkErrorWire, WorkFailureWire, WorkOperationWire,
    },
};
use brainprint_engine::{
    git_observation::{GitEntryStatus, GitObservation},
    git_status::{self, GitStatus, GitStatusError},
};

use super::DaemonQueryRuntime;

/// Replace `Observe` with what Git reported. Anything else passes through
/// untouched (no Git call).
pub(super) async fn resolve(
    runtime: &DaemonQueryRuntime,
    workspace: WorkspaceId,
    operation: WorkOperationWire,
) -> Result<WorkOperationWire, WorkFailureWire> {
    let wants = match &operation {
        WorkOperationWire::Start(start) => start.git == GitObservationWire::Observe,
        WorkOperationWire::Result(input) => input.git == GitObservationWire::Observe,
    };
    if !wants {
        return Ok(operation);
    }
    if let WorkOperationWire::Start(start) = &operation
        && start.head.is_some()
    {
        return Err(failure(WorkErrorWire::InvalidObservation {
            reason: "Observe supplies head itself; leave head empty".to_owned(),
        }));
    }
    let root = runtime
        .workspace_root(workspace)
        .await
        .map_err(|error| failure(WorkErrorWire::Workspace(error)))?;
    let status = tokio::task::spawn_blocking(move || git_status::observe(&root))
        .await
        .map_err(|_| {
            failure(WorkErrorWire::Internal {
                message: "git observation task panicked".to_owned(),
            })
        })?
        .map_err(|error| failure(WorkErrorWire::GitObservation(failure_wire(error))))?;
    Ok(apply(operation, status))
}

fn apply(operation: WorkOperationWire, status: GitStatus) -> WorkOperationWire {
    let git = observation_wire(status.observation);
    match operation {
        WorkOperationWire::Start(mut start) => {
            start.head = status.head;
            start.git = git;
            WorkOperationWire::Start(start)
        }
        WorkOperationWire::Result(mut input) => {
            input.git = git;
            WorkOperationWire::Result(input)
        }
    }
}

fn observation_wire(observation: GitObservation) -> GitObservationWire {
    match observation {
        GitObservation::Unknown => GitObservationWire::Unknown,
        GitObservation::Clean => GitObservationWire::Clean,
        GitObservation::Dirty(entries) => GitObservationWire::Dirty {
            entries: entries
                .into_iter()
                .map(|entry| GitEntryWire {
                    status: match entry.status {
                        GitEntryStatus::Added => GitEntryStatusWire::Added,
                        GitEntryStatus::Modified => GitEntryStatusWire::Modified,
                        GitEntryStatus::Deleted => GitEntryStatusWire::Deleted,
                        GitEntryStatus::Renamed => GitEntryStatusWire::Renamed,
                        GitEntryStatus::Untracked => GitEntryStatusWire::Untracked,
                        GitEntryStatus::Conflicted => GitEntryStatusWire::Conflicted,
                    },
                    path: entry.path,
                    old_path: entry.old_path,
                })
                .collect(),
        },
    }
}

/// Detail (stderr, spawn error) goes to the daemon log only.
fn failure_wire(error: GitStatusError) -> GitObservationFailureWire {
    match error {
        GitStatusError::NotAGitWorkspace => GitObservationFailureWire::NotAGitWorkspace,
        GitStatusError::WorkspaceBoundaryMismatch => {
            GitObservationFailureWire::WorkspaceBoundaryMismatch
        }
        GitStatusError::GitUnavailable(detail) => {
            eprintln!("brainprintd: git observation: git could not start: {detail}");
            GitObservationFailureWire::GitUnavailable
        }
        GitStatusError::GitFailed { exit_code, stderr } => {
            eprintln!("brainprintd: git observation: git exited {exit_code:?}: {stderr}");
            GitObservationFailureWire::GitFailed { exit_code }
        }
        GitStatusError::Timeout => GitObservationFailureWire::Timeout,
        GitStatusError::OutputTooLarge => GitObservationFailureWire::OutputTooLarge,
        GitStatusError::TooManyEntries => GitObservationFailureWire::TooManyEntries,
        GitStatusError::UnrepresentablePath => GitObservationFailureWire::UnrepresentablePath,
        GitStatusError::Unparsable(what) => {
            eprintln!("brainprintd: git observation: unparsable status: {what}");
            GitObservationFailureWire::Unparsable
        }
        GitStatusError::ChangedDuringObservation => {
            GitObservationFailureWire::ChangedDuringObservation
        }
    }
}

const fn failure(error: WorkErrorWire) -> WorkFailureWire {
    super::work::failure(error)
}
