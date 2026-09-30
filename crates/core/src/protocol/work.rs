//! Working State write wire (#50, I6 task 1): caller-observed Git state
//! delivered to the #20 task 3 lifecycle.
//!
//! The caller observed HEAD, the changed entries and any verification
//! summary; the write itself runs no command (#51 `Observe` and #52
//! `verification` run before it). It fingerprints the entries, maps paths
//! onto ACTIVE Resources where index.db has them, and stores the
//! observation through the existing `WorkRuntime`.

use serde::{Deserialize, Serialize};

use crate::{
    WorkItemId, WorkspaceId,
    protocol::query::{
        QueryErrorWire, WorkItemSourceKindWire, WorkItemStatusWire, WorkResultWire,
        WorkingStateWire, WorkspaceSelectorWire,
    },
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkRequest {
    pub workspace: WorkspaceSelectorWire,
    pub operation: WorkOperationWire,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkOperationWire {
    Start(WorkStartWire),
    Result(WorkResultInputWire),
}

/// OPEN → ACTIVE with the baseline observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkStartWire {
    pub work_item: WorkStartItemWire,
    /// The commit id the caller observed.
    pub head: Option<String>,
    pub git: GitObservationWire,
    pub owner_agent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkStartItemWire {
    /// An OPEN WorkItem.
    Existing(WorkItemId),
    /// Created only once the Workspace can take a baseline.
    New(NewWorkItemWire),
}

/// Mirrors `brainprint_engine::knowledge::NewWorkItem`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewWorkItemWire {
    pub source_kind: WorkItemSourceKindWire,
    pub source_ref: Option<String>,
    pub title: Option<String>,
    pub goal: String,
}

/// UNKNOWN (did not look) / CLEAN (looked, nothing) / DIRTY (entries).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitObservationWire {
    Unknown,
    Clean,
    Dirty {
        entries: Vec<GitEntryWire>,
    },
    /// #51: the daemon reads the Workspace's Git state itself (v5). On a
    /// Start it also supplies `head`, so the caller's `head` must be empty.
    /// Two consecutive status reads must agree; that is not an atomic
    /// snapshot of the work tree.
    Observe,
}

/// Mirrors `brainprint_engine::git_observation::GitEntry`. `path` is
/// Workspace-root relative; `old_path` is set exactly for `Renamed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitEntryWire {
    pub status: GitEntryStatusWire,
    pub path: String,
    pub old_path: Option<String>,
}

/// Mirrors `brainprint_engine::git_observation::GitEntryStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitEntryStatusWire {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkOutcomeWire {
    /// Checkpoint; the WorkItem keeps its status.
    Partial,
    /// ACTIVE → COMPLETED.
    Complete,
    /// Any non-terminal status → ABANDONED.
    Abandon,
}

/// A result as the caller observed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkResultInputWire {
    pub work_item: WorkItemId,
    pub outcome: WorkOutcomeWire,
    pub summary: String,
    pub commit_id: Option<String>,
    /// Stored as given; nothing is run to produce or check it. Never
    /// together with `verification`.
    pub verification_summary: Option<String>,
    /// #52: caller-named commands the daemon runs before the write; their
    /// compact summary becomes `verification_summary`.
    pub verification: Option<VerificationWire>,
    /// Remaining dirty state after the task.
    pub git: GitObservationWire,
    /// What the task changed; fingerprinted like `git` entries.
    pub change_set: Option<Vec<GitEntryWire>>,
}

/// #52: the commands to run, in order. Mirrors
/// `brainprint_engine::verification::VerificationCommand`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationWire {
    pub commands: Vec<VerificationCommandWire>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationCommandWire {
    pub label: String,
    /// `argv[0]` is the program; no shell.
    pub argv: Vec<String>,
    /// Workspace-relative; `None` is the Workspace root.
    pub cwd: Option<String>,
    /// Overrides on top of the daemon's environment.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    pub timeout_secs: u32,
}

/// One command's result. argv, env, cwd and output are never echoed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandResultWire {
    pub label: String,
    pub outcome: VerificationOutcomeWire,
    pub duration_ms: u64,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
}

/// Mirrors `brainprint_engine::verification::VerificationOutcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerificationOutcomeWire {
    Passed,
    Failed {
        exit_code: i32,
    },
    /// Ended by a signal Brainprint did not send (unix).
    Signaled {
        signal: i32,
    },
    TimedOut,
    NotStarted {
        reason: NotStartedReasonWire,
    },
    /// An earlier command did not pass; this one never ran.
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotStartedReasonWire {
    NotFound,
    PermissionDenied,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkResponse {
    Started(WorkStartedWire),
    Recorded(WorkRecordedWire),
    Failed(WorkFailureWire),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkStartedWire {
    pub workspace_id: WorkspaceId,
    pub working_state: WorkingStateWire,
    /// Entry paths index.db holds no ACTIVE Resource for. No ResourceID is
    /// made for them; they are still part of the fingerprint.
    pub unresolved_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkRecordedWire {
    pub workspace_id: WorkspaceId,
    pub status: WorkItemStatusWire,
    pub result: WorkResultWire,
    /// #52: `Some` exactly when the request's verification ran.
    pub verification: Option<Vec<CommandResultWire>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkFailureWire {
    pub error: WorkErrorWire,
    /// Set only if a `New` WorkItem was created and the start after it
    /// failed: it is OPEN, and a retry names it as `Existing`.
    pub created_work_item: Option<WorkItemId>,
    /// #52: `Some` means the commands ran but nothing was stored.
    pub verification: Option<Vec<CommandResultWire>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkErrorWire {
    /// Selector, registration or binding failure: the query mapping.
    Workspace(QueryErrorWire),
    InvalidObservation {
        reason: String,
    },
    BoundExceeded {
        what: String,
    },
    WorkItemNotFound {
        work_item: WorkItemId,
    },
    InvalidTransition {
        reason: String,
    },
    /// Retryable: nothing was written.
    WorkspaceNotReady(WorkNotReadyWire),
    /// #51: `Observe` produced no observation; nothing was written.
    GitObservation(GitObservationFailureWire),
    /// #52: a verification already runs for this Workspace, or the daemon
    /// runs its limit. Nothing ran or was written; not queued.
    VerificationBusy,
    /// A safe summary; storage detail stays in the daemon log.
    Internal {
        message: String,
    },
}

/// Mirrors `brainprint_engine::knowledge::NotReady`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkNotReadyWire {
    ClockNotBootstrapped,
    NoStableGeneration,
    StableBehind {
        stable_basis: String,
        current: String,
    },
}

/// Mirrors `brainprint_engine::git_status::GitStatusError`, without the
/// daemon-log-only detail (stderr, spawn error text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitObservationFailureWire {
    NotAGitWorkspace,
    /// Git's work tree is not the Workspace root.
    WorkspaceBoundaryMismatch,
    GitUnavailable,
    GitFailed {
        exit_code: Option<i32>,
    },
    Timeout,
    OutputTooLarge,
    TooManyEntries,
    UnrepresentablePath,
    Unparsable,
    /// The two status reads differed. Retryable.
    ChangedDuringObservation,
}
