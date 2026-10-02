//! #54 step 3: managed verification Jobs over local IPC -- start one,
//! poll its durable events, cancel it. A thin view of the daemon's Job
//! lifecycle; argv, env, cwd and raw output never cross it.

use serde::{Deserialize, Serialize};

use crate::{
    VerificationJobId,
    protocol::{
        query::{QueryErrorWire, WorkspaceSelectorWire},
        work::{
            CommandResultWire, PostCommandRefreshWire, RawAvailabilityWire, StreamStatusWire,
            VerificationOutcomeWire, VerificationWire,
        },
    },
};

/// The most events one poll asks for.
pub const MAX_POLL_EVENTS: u32 = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationJobStartRequestWire {
    pub workspace: WorkspaceSelectorWire,
    /// The caller's; the same key with the same request names the same
    /// Job, so a retry after a lost reply runs nothing again.
    pub idempotency_key: String,
    pub verification: VerificationWire,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationJobStartedWire {
    pub job_id: VerificationJobId,
    pub state: VerificationJobStateWire,
    /// The key named an existing Job: returned as it is, nothing run.
    pub replayed: bool,
    pub last_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationJobPollRequestWire {
    pub workspace: WorkspaceSelectorWire,
    pub job_id: VerificationJobId,
    /// Events with `seq > after_seq`; 0 for all.
    pub after_seq: u64,
    /// `1..=MAX_POLL_EVENTS`.
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationJobPollWire {
    pub job_id: VerificationJobId,
    /// Read before the events: a terminal state here comes with every
    /// event up to its terminal one.
    pub state: VerificationJobStateWire,
    /// Ascending; fewer than `limit` when the next would not fit the frame.
    pub events: Vec<VerificationJobEventWire>,
    /// The next poll's `after_seq`.
    pub next_seq: u64,
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationJobCancelRequestWire {
    pub workspace: WorkspaceSelectorWire,
    pub job_id: VerificationJobId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationJobCancelledWire {
    pub job_id: VerificationJobId,
    /// The stored terminal state: `Cancelled`, or whichever came first.
    pub state: VerificationJobStateWire,
}

/// `Finished` is "ran to its end", not "passed": each command's outcome
/// is in its events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerificationJobStateWire {
    Running,
    Finished,
    Cancelled,
    Interrupted,
    InternalError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationJobEventWire {
    pub seq: u64,
    pub created_at: String,
    pub payload: VerificationJobEventPayloadWire,
}

/// The durable event payloads, as stored. #55: a terminal event carries
/// its post-command refresh when one was recorded; `None` for a Job from
/// before #55, one an earlier daemon left running, or one whose
/// pre-command baseline failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerificationJobEventPayloadWire {
    JobStarted,
    /// `index` is 0-based, the batch order.
    CommandStarted {
        index: u32,
        label: String,
    },
    CommandFinished(CommandFinishedWire),
    /// The #53-shaped results, diagnostics included once.
    JobFinished {
        verification_summary: String,
        results: Vec<CommandResultWire>,
        refresh: Option<PostCommandRefreshWire>,
    },
    JobCancelled {
        reason: JobEndReasonWire,
        refresh: Option<PostCommandRefreshWire>,
    },
    JobInterrupted {
        reason: JobEndReasonWire,
        refresh: Option<PostCommandRefreshWire>,
    },
    JobInternalError {
        reason: JobEndReasonWire,
        refresh: Option<PostCommandRefreshWire>,
    },
}

/// Progress facts only; diagnostic items wait for `JobFinished`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandFinishedWire {
    pub index: u32,
    pub label: String,
    pub outcome: VerificationOutcomeWire,
    pub duration_ms: u64,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub capture: CaptureProgressWire,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureProgressWire {
    NotRequested,
    NotRun,
    Captured {
        stream_status: StreamStatusWire,
        diagnostics: Option<DiagnosticCountsWire>,
        /// `Unavailable` once the daemon no longer holds the artifact.
        raw: RawAvailabilityWire,
    },
}

/// The parser's counts as the command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticCountsWire {
    pub observed: u64,
    pub deduplicated: u64,
    pub omitted: u64,
    pub parse_misses: u64,
    /// Items kept by the parser.
    pub retained: u64,
}

/// Why a Job ended other than `Finished`; a closed category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobEndReasonWire {
    CallerCancelled,
    DaemonShutdown,
    DaemonRestart,
    EventPersistence,
    EventPayload,
    RunnerFailure,
    /// #55: no current index basis before any command; nothing ran.
    BaselineCurrentness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerificationJobErrorWire {
    /// Selector, registration or binding failure: the query mapping.
    Workspace(QueryErrorWire),
    /// The batch was refused before anything ran.
    InvalidVerification {
        reason: String,
    },
    InvalidIdempotencyKey,
    /// The key names a Job with a different request; nothing ran.
    IdempotencyConflict,
    /// A verification already runs for this Workspace, or the daemon runs
    /// its limit. No Job was created; the key stays free.
    VerificationBusy,
    JobNotFound,
    /// `limit` outside `1..=MAX_POLL_EVENTS`.
    InvalidLimit,
    /// A stored Job or event outside its shape.
    Corrupt,
    /// A safe summary; detail stays in the daemon log.
    Internal {
        message: String,
    },
}

pub type VerificationJobStartResponseWire =
    Result<VerificationJobStartedWire, VerificationJobErrorWire>;
pub type VerificationJobPollResponseWire =
    Result<VerificationJobPollWire, VerificationJobErrorWire>;
pub type VerificationJobCancelResponseWire =
    Result<VerificationJobCancelledWire, VerificationJobErrorWire>;
