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
    ResourceId, VerificationJobId, WorkItemId, WorkspaceId,
    protocol::query::{
        QueryErrorWire, WorkItemSourceKindWire, WorkItemStatusWire, WorkResultWire,
        WorkingStateWire, WorkspaceSelectorWire,
    },
    protocol::verification_job::VerificationJobStateWire,
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
    /// #55: a FINISHED managed Job of this Workspace, recorded as the
    /// verification without running anything. At most one of
    /// `verification_summary`, `verification` and `verification_job`.
    #[serde(default)]
    pub verification_job: Option<VerificationJobId>,
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
    /// #53: what to capture of this command's output; `None` keeps none
    /// (#52). Asking for nothing (`raw: false`, no diagnostics) refuses
    /// the batch.
    #[serde(default)]
    pub capture: Option<VerificationCaptureWire>,
}

/// #53: an explicit capture request. Brainprint never picks a parser by
/// looking at the program, argv or output, and never adds argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationCaptureWire {
    /// Keep the head and tail of each stream as an ephemeral artifact.
    pub raw: bool,
    pub diagnostics: Option<DiagnosticFormatWire>,
}

/// Mirrors `brainprint_engine::diagnostics::DiagnosticFormat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticFormatWire {
    /// Cargo's `--message-format=json` `compiler-message` lines only; a
    /// direct `rustc --error-format=json` line is a parse miss.
    CargoCompilerMessageJson,
    /// `<path>:<line>:<column>: <message>`.
    PathLineColumn,
}

/// One command's result. argv, env, cwd and output are never echoed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandResultWire {
    pub label: String,
    pub outcome: VerificationOutcomeWire,
    pub duration_ms: u64,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    /// #53: never changes `outcome`.
    pub capture: CommandCaptureWire,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandCaptureWire {
    /// The command had no capture request.
    NotRequested,
    /// Requested, but the command never spawned (`NotStarted`, `Skipped`).
    NotRun,
    /// The command spawned and its output was captured.
    Captured {
        stream_status: StreamStatusWire,
        diagnostics: Option<DiagnosticSummaryWire>,
        raw: RawAvailabilityWire,
    },
}

/// Whether both streams were read to EOF; says nothing about storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamStatusWire {
    Complete,
    /// A stream ended with the run (deadline, a descendant holding the
    /// pipe): what was captured stops there.
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RawAvailabilityWire {
    NotRequested,
    Available(RawArtifactRefWire),
    /// Requested, but no usable handle: no room, a storage failure, or
    /// already evicted when the response was built.
    Unavailable,
}

/// An ephemeral raw artifact: read it with `Request::ArtifactRead` while
/// the daemon keeps it (bounded LRU; a restart expires every handle).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawArtifactRefWire {
    /// Opaque; names no path, Workspace, argv or label.
    pub handle: String,
    pub stdout: RawStreamMetaWire,
    pub stderr: RawStreamMetaWire,
}

/// One stream's retained parts. Not truncated: all of it is the head and
/// the tail is empty. Truncated: head and tail are `omitted_bytes` apart
/// in the original stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawStreamMetaWire {
    pub observed_bytes: u64,
    pub head_bytes: u64,
    pub tail_bytes: u64,
    pub omitted_bytes: u64,
    pub truncated: bool,
    pub tail_start_offset: u64,
}

/// Mirrors `brainprint_engine::diagnostics::DiagnosticSummary`, plus what
/// the response's diagnostic budget left out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticSummaryWire {
    pub items: Vec<DiagnosticWire>,
    /// Diagnostics parsed, duplicates included.
    pub observed: u64,
    /// Exact duplicates of a kept item; not a global unique count.
    pub deduplicated: u64,
    /// Beyond the per-command bound of 64.
    pub omitted: u64,
    /// Lines that are not a diagnostic of the format.
    pub parse_misses: u64,
    /// Kept by the parser but left out of this response by its 512 KiB
    /// diagnostic budget. Never counted in `omitted`.
    pub delivery_omitted: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticWire {
    pub severity: DiagnosticSeverityWire,
    pub code: Option<String>,
    /// At most 2 KiB, cut at a UTF-8 boundary (`message_truncated`).
    pub message: String,
    pub path: DiagnosticPathWire,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub stream: OutputStreamWire,
    pub message_truncated: bool,
}

/// In delivery priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticSeverityWire {
    Error,
    Warning,
    Note,
    Help,
    Unknown,
}

/// Mirrors `brainprint_engine::diagnostics::DiagnosticPath`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticPathWire {
    /// The diagnostic named no path.
    Absent,
    /// An existing file in the Workspace, `/`-separated and relative.
    Workspace { path: String },
    /// An existing file outside the Workspace; its text is not sent.
    External,
    /// Named, but not an existing file (virtual, deleted, missing, a
    /// directory); its text is not sent.
    Unresolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputStreamWire {
    Stdout,
    Stderr,
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
    /// #55: `Some` exactly when the request's verification ran: the
    /// Workspace's currentness after its commands. A managed Job's is on
    /// the Job's terminal event.
    pub refresh: Option<PostCommandRefreshWire>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkFailureWire {
    pub error: WorkErrorWire,
    /// Set only if a `New` WorkItem was created and the start after it
    /// failed: it is OPEN, and a retry names it as `Existing`.
    pub created_work_item: Option<WorkItemId>,
    /// #52: `Some` means the commands ran but nothing was stored.
    pub verification: Option<Vec<CommandResultWire>>,
    /// #55: the post-command refresh of commands that ran.
    pub refresh: Option<PostCommandRefreshWire>,
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
    /// #55: no current index basis before the commands; none ran.
    VerificationBaselineNotCurrent,
    /// #55: the commands ran, but no current basis after them could be
    /// proven; nothing was written (`refresh` says so too).
    VerificationRefreshFailed {
        reason: RefreshFailureWire,
    },
    /// #55: the Workspace moved past the verified basis before the write;
    /// nothing was written.
    VerificationResultBasisChanged,
    /// #55: `verification_job` names no Job of this Workspace.
    VerificationJobNotFound,
    /// #55: only a FINISHED Job is a finished verification.
    VerificationJobNotFinished {
        state: VerificationJobStateWire,
    },
    /// #55: a Job from before post-command freshness evidence existed.
    VerificationJobNoRefreshBasis,
    /// #55: the Job's own refresh proved no current basis.
    VerificationJobRefreshNotCurrent,
    /// #55: the Workspace has moved past the Job's basis.
    VerificationJobStale,
    /// #55: the stored Job or its terminal event is outside its shape.
    VerificationJobCorrupt,
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

/// #55: a proven-current index basis: which index.db incarnation, at which
/// Workspace revision, on which STABLE generation (built for exactly that
/// revision).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexBasisWire {
    pub index_incarnation: crate::IndexIncarnationId,
    pub workspace_revision: String,
    pub generation_no: i64,
    pub generation_basis_revision: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceDeltaKindWire {
    Created,
    Updated,
    Deleted,
}

/// One Resource that differs between the pre-command basis and the
/// post-command one -- not a claim that the command changed it. A move
/// is `Updated`; for `Deleted`, the tombstone's path and revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceDeltaWire {
    pub resource_id: ResourceId,
    pub kind: ResourceDeltaKindWire,
    pub path: String,
    pub resource_revision: String,
}

/// #55: the Workspace's currentness after a command batch. A fact beside
/// the command outcomes, never one of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PostCommandRefreshWire {
    /// No process was spawned: the pre-command basis still holds.
    NotNeeded { basis: IndexBasisWire },
    /// A verified refresh proved `after` current. Counts are exact;
    /// `changes` is a deterministic prefix within 256 KiB and
    /// `delivery_omitted` the rest.
    Current {
        before: IndexBasisWire,
        after: IndexBasisWire,
        created_count: u64,
        updated_count: u64,
        deleted_count: u64,
        total_changed: u64,
        changes: Vec<ResourceDeltaWire>,
        delivery_omitted: u64,
    },
    /// No current basis could be proven; detail is in the daemon log.
    Failed {
        before: Option<IndexBasisWire>,
        reason: RefreshFailureWire,
    },
    /// The daemon shut down first; the next one's freshness barrier
    /// recovers.
    DeferredDaemonShutdown { before: Option<IndexBasisWire> },
}

/// Why a post-command refresh proved nothing. A closed category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefreshFailureWire {
    WorkspaceUnavailable,
    ReconcileFailed,
    NotCurrentAfterRefresh,
    StableBasisMismatch,
    IndexIncarnationChanged,
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IndexIncarnationId, protocol::verification_job::*};

    fn round_trip<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(
        value: &T,
    ) {
        let json = serde_json::to_string(value).expect("encode");
        assert_eq!(
            &serde_json::from_str::<T>(&json).expect("decode"),
            value,
            "{json}"
        );
    }

    fn basis(revision: &str) -> IndexBasisWire {
        IndexBasisWire {
            index_incarnation: IndexIncarnationId::generate(),
            workspace_revision: revision.to_owned(),
            generation_no: 7,
            generation_basis_revision: revision.to_owned(),
        }
    }

    /// #55: every new protocol 9 shape survives the wire as it is.
    #[test]
    fn protocol_9_refresh_and_provenance_round_trip() {
        let current = PostCommandRefreshWire::Current {
            before: basis("1"),
            after: basis("2"),
            created_count: 1,
            updated_count: 2,
            deleted_count: 3,
            total_changed: 6,
            changes: vec![ResourceDeltaWire {
                resource_id: ResourceId::generate(),
                kind: ResourceDeltaKindWire::Deleted,
                path: "src/a.rs".to_owned(),
                resource_revision: "9".to_owned(),
            }],
            delivery_omitted: 5,
        };
        for refresh in [
            current.clone(),
            PostCommandRefreshWire::NotNeeded { basis: basis("1") },
            PostCommandRefreshWire::Failed {
                before: None,
                reason: RefreshFailureWire::StableBasisMismatch,
            },
            PostCommandRefreshWire::DeferredDaemonShutdown {
                before: Some(basis("3")),
            },
        ] {
            round_trip(&refresh);
        }
        for payload in [
            VerificationJobEventPayloadWire::JobFinished {
                verification_summary: "s".to_owned(),
                results: Vec::new(),
                refresh: Some(current.clone()),
            },
            VerificationJobEventPayloadWire::JobCancelled {
                reason: JobEndReasonWire::CallerCancelled,
                refresh: Some(current),
            },
            VerificationJobEventPayloadWire::JobInterrupted {
                reason: JobEndReasonWire::DaemonShutdown,
                refresh: Some(PostCommandRefreshWire::DeferredDaemonShutdown { before: None }),
            },
            VerificationJobEventPayloadWire::JobInternalError {
                reason: JobEndReasonWire::BaselineCurrentness,
                refresh: None,
            },
        ] {
            round_trip(&payload);
        }
        for error in [
            WorkErrorWire::VerificationBaselineNotCurrent,
            WorkErrorWire::VerificationRefreshFailed {
                reason: RefreshFailureWire::ReconcileFailed,
            },
            WorkErrorWire::VerificationResultBasisChanged,
            WorkErrorWire::VerificationJobNotFound,
            WorkErrorWire::VerificationJobNotFinished {
                state: VerificationJobStateWire::Interrupted,
            },
            WorkErrorWire::VerificationJobNoRefreshBasis,
            WorkErrorWire::VerificationJobRefreshNotCurrent,
            WorkErrorWire::VerificationJobStale,
            WorkErrorWire::VerificationJobCorrupt,
        ] {
            round_trip(&error);
        }
        let input = WorkResultInputWire {
            work_item: WorkItemId::generate(),
            outcome: WorkOutcomeWire::Partial,
            summary: "s".to_owned(),
            commit_id: None,
            verification_summary: None,
            verification: None,
            verification_job: Some(VerificationJobId::generate()),
            git: GitObservationWire::Observe,
            change_set: None,
        };
        round_trip(&input);
        // Strict shapes stay strict.
        let json = serde_json::to_string(&basis("1")).expect("encode");
        let extra = json.replacen('{', "{\"extra\":1,", 1);
        assert!(serde_json::from_str::<IndexBasisWire>(&extra).is_err());
    }
}
