//! #54 step 2: daemon-managed verification Jobs -- the #52 batch run in
//! the background, its lifecycle a durable event log in the Workspace's
//! workspace.db, independent of the connection that started it.
//!
//! - Start: the whole request is checked and its idempotency key looked
//!   up before a shared #52 slot is taken; a busy daemon creates no Job.
//!   A replayed key (same request) returns the existing Job, whatever its
//!   state, and runs nothing.
//! - Each command: `COMMAND_STARTED` right before its spawn attempt,
//!   `COMMAND_FINISHED` once its result and #53 capture are final. A
//!   failed event write stops the batch (`INTERNAL_ERROR`).
//! - The in-memory registry holds only the Jobs this daemon runs. A
//!   Workspace's first managed use reconciles whatever an earlier daemon
//!   left RUNNING to INTERRUPTED, once per daemon; nothing is rerun.
//! - Event payloads are versioned typed structs, never caller text; argv,
//!   env, cwd and raw output are never stored.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use brainprint_core::{
    VerificationJobId, WorkspaceId,
    protocol::{
        query::QueryErrorWire,
        work::{
            CommandResultWire, RawAvailabilityWire, StreamStatusWire, VerificationOutcomeWire,
            VerificationWire,
        },
    },
};
use brainprint_engine::{
    output_capture::{CaptureRequest, CommandCapture},
    paths::WorkspacePaths,
    process_runner::Cancel,
    query_surface::{CoreError, NotInitialized},
    verification::{
        self, CaptureConsumer as _, CommandResult, ManagedObserver, ManagedStop, ObserverFailed,
        PreparedVerification,
    },
    verification_job::{
        IdempotencyKey, MAX_EVENTS_PER_READ, NewVerificationJob, ProgressEvent, TerminalState,
        Transition, VerificationJob, VerificationJobError, VerificationJobEvent,
        VerificationJobEventKind, VerificationJobState, VerificationJobStore, request_fingerprint,
    },
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::sync::watch;

use super::{
    DaemonQueryRuntime,
    capture::{self, Kept, StoreConsumer, Undelivered},
    convert_out,
    runtime::MAX_RESULT_BYTES,
    verify::{self, Slot},
};
use crate::artifacts::ArtifactStore;

/// The version every stored event payload carries as `v`. A new payload
/// meaning gets a new version; old rows are never reinterpreted.
pub const MANAGED_JOB_EVENT_PAYLOAD_VERSION: u32 = 1;
/// A serialized payload stays below this: the 1 MiB frame less the query
/// runtime's 4 KiB envelope headroom. Not a setting.
pub const MAX_PAYLOAD_BYTES: usize = MAX_RESULT_BYTES;
const V: u32 = MANAGED_JOB_EVENT_PAYLOAD_VERSION;

// ------------------------------------------------------------- payloads

/// `JOB_STARTED`: nothing from the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobStartedPayload {
    pub v: u32,
}

/// `COMMAND_STARTED`: `index` is 0-based, the batch order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandStartedPayload {
    pub v: u32,
    pub index: u32,
    pub label: String,
}

/// `COMMAND_FINISHED`: progress facts only; diagnostic items wait for
/// `JOB_FINISHED`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandFinishedPayload {
    pub v: u32,
    pub index: u32,
    pub label: String,
    pub outcome: VerificationOutcomeWire,
    pub duration_ms: u64,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub capture: CaptureProgress,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureProgress {
    NotRequested,
    NotRun,
    Captured {
        stream_status: StreamStatusWire,
        diagnostics: Option<DiagnosticCounts>,
        raw: RawAvailabilityWire,
    },
}

/// The parser's counts as the command ended; no response-wide delivery
/// count exists yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticCounts {
    pub observed: u64,
    pub deduplicated: u64,
    pub omitted: u64,
    pub parse_misses: u64,
    /// Items kept by the parser.
    pub retained: u64,
}

/// `JOB_FINISHED`: the #53-shaped results, diagnostics included once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobFinishedPayload {
    pub v: u32,
    pub verification_summary: String,
    pub results: Vec<CommandResultWire>,
}

/// `JOB_CANCELLED` / `JOB_INTERRUPTED` / `JOB_INTERNAL_ERROR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobEndedPayload {
    pub v: u32,
    pub reason: EndReason,
}

/// Why a Job ended other than `FINISHED`. A closed category, never error
/// text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// `CANCELLED`: the caller's explicit cancel.
    CallerCancelled,
    /// `INTERRUPTED`: this daemon shut down cleanly.
    DaemonShutdown,
    /// `INTERRUPTED`: an earlier daemon left it RUNNING.
    DaemonRestart,
    /// `INTERNAL_ERROR`: an event could not be stored.
    EventPersistence,
    /// `INTERNAL_ERROR`: an event payload could not be encoded within
    /// [`MAX_PAYLOAD_BYTES`].
    EventPayload,
    /// `INTERNAL_ERROR`: the run itself failed (panicked).
    RunnerFailure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedEventPayload {
    JobStarted(JobStartedPayload),
    CommandStarted(CommandStartedPayload),
    CommandFinished(CommandFinishedPayload),
    JobFinished(JobFinishedPayload),
    JobCancelled(JobEndedPayload),
    JobInterrupted(JobEndedPayload),
    JobInternalError(JobEndedPayload),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedEvent {
    pub seq: u64,
    pub created_at: String,
    pub payload: ManagedEventPayload,
}

fn encode(payload: &impl Serialize) -> Result<String, EndReason> {
    match serde_json::to_string(payload) {
        Ok(json) if json.len() < MAX_PAYLOAD_BYTES => Ok(json),
        _ => Err(EndReason::EventPayload),
    }
}

fn ended(reason: EndReason) -> String {
    encode(&JobEndedPayload { v: V, reason }).expect("a fixed payload fits")
}

/// A stored payload read back: `v` first -- an unknown version is never
/// read as this one -- then the exact shape of its kind.
fn decode(event: &VerificationJobEvent) -> Result<ManagedEventPayload, ManagedError> {
    #[derive(Deserialize)]
    struct Version {
        v: u32,
    }
    fn parse<T: DeserializeOwned>(json: &str) -> Result<T, ManagedError> {
        serde_json::from_str(json).map_err(|_| ManagedError::Corrupt)
    }
    let json = event.payload_json.as_str();
    if parse::<Version>(json)?.v != V {
        return Err(ManagedError::Corrupt);
    }
    use ManagedEventPayload as P;
    use VerificationJobEventKind as K;
    Ok(match event.kind {
        K::JobStarted => P::JobStarted(parse(json)?),
        K::CommandStarted => P::CommandStarted(parse(json)?),
        K::CommandFinished => P::CommandFinished(parse(json)?),
        K::JobFinished => P::JobFinished(parse(json)?),
        K::JobCancelled => P::JobCancelled(parse(json)?),
        K::JobInterrupted => P::JobInterrupted(parse(json)?),
        K::JobInternalError => P::JobInternalError(parse(json)?),
    })
}

/// A stored raw handle is a hint, not the artifact: one the store no
/// longer holds reads as `Unavailable`. The durable outcome is untouched.
fn downgrade(payload: &mut ManagedEventPayload, store: &ArtifactStore) {
    match payload {
        ManagedEventPayload::CommandFinished(CommandFinishedPayload {
            capture: CaptureProgress::Captured { raw, .. },
            ..
        }) => {
            if let RawAvailabilityWire::Available(reference) = raw
                && !store.contains(&reference.handle)
            {
                *raw = RawAvailabilityWire::Unavailable;
            }
        }
        ManagedEventPayload::JobFinished(finished) => {
            capture::downgrade_evicted(&mut finished.results, store);
        }
        _ => {}
    }
}

// ------------------------------------------------------------- results

/// What a start did; `replayed`: the key named an existing Job, returned
/// as it is with nothing run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagedStart {
    pub job_id: VerificationJobId,
    pub state: VerificationJobState,
    pub replayed: bool,
    pub last_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedPoll {
    pub job: VerificationJob,
    /// `seq > after_seq`, ascending.
    pub events: Vec<ManagedEvent>,
    /// The cursor for the next poll: the last event's seq, else
    /// `after_seq`.
    pub next_seq: u64,
    pub has_more: bool,
}

/// A refused managed request. Typed so step 3 can map each to the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedError {
    /// Not registered, not initialized or not bound.
    Workspace(QueryErrorWire),
    /// The batch failed `prepare`: nothing ran.
    InvalidVerification(String),
    InvalidIdempotencyKey,
    /// The key names a Job with a different request.
    IdempotencyConflict,
    /// No shared #52 slot: no Job was created, the key stays free.
    VerificationBusy,
    JobNotFound,
    /// `limit` outside 1..=64.
    InvalidLimit,
    /// A stored row or payload outside its shape.
    Corrupt,
    Internal(&'static str),
}

fn storage(error: VerificationJobError) -> ManagedError {
    let workspace = |error| ManagedError::Workspace(convert_out::core_error(error));
    match error {
        VerificationJobError::MissingDatabase => workspace(CoreError::NotInitialized(
            NotInitialized::WorkspaceDbMissing,
        )),
        VerificationJobError::UnboundWorkspace => workspace(CoreError::NotInitialized(
            NotInitialized::WorkspaceUnbound { db: "workspace.db" },
        )),
        VerificationJobError::WorkspaceMismatch { expected, found } => {
            workspace(CoreError::WorkspaceBindingMismatch {
                db: "workspace.db",
                expected,
                found,
            })
        }
        VerificationJobError::NotFound => ManagedError::JobNotFound,
        VerificationJobError::InvalidLimit(_) => ManagedError::InvalidLimit,
        VerificationJobError::Corrupt(_) => ManagedError::Corrupt,
        // A category only: driver text may name files.
        _ => {
            eprintln!("brainprintd: managed verification: job storage failed");
            ManagedError::Internal("verification job storage failed")
        }
    }
}

// ------------------------------------------------------------- registry

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    None,
    CallerCancelled,
    DaemonShutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Done {
    Running,
    /// `None`: no terminal state could be stored; the row stays RUNNING
    /// until a later daemon reconciles it.
    Ended(Option<VerificationJobState>),
}

/// One Job this daemon runs.
struct Control {
    cancel: Cancel,
    /// One-way: the first stop intent wins.
    reason: Mutex<StopReason>,
    done: watch::Sender<Done>,
}

impl Control {
    fn reason(&self) -> MutexGuard<'_, StopReason> {
        self.reason.lock().expect("stop reason mutex poisoned")
    }

    fn stop(&self, reason: StopReason) {
        {
            let mut current = self.reason();
            if *current == StopReason::None {
                *current = reason;
            }
        }
        self.cancel.cancel();
    }

    /// Once the Job is terminal (or could not be made so) and its slot
    /// freed.
    async fn ended(&self) -> Option<VerificationJobState> {
        let mut done = self.done.subscribe();
        match done.wait_for(|done| *done != Done::Running).await {
            Ok(done) => match *done {
                Done::Ended(state) => state,
                Done::Running => None,
            },
            Err(_) => None,
        }
    }
}

type Key = (WorkspaceId, VerificationJobId);

#[derive(Default)]
struct Registry {
    jobs: HashMap<Key, Arc<Control>>,
    /// Clean shutdown began: no new Job.
    closed: bool,
}

/// The daemon's managed-Job state: the one-time Workspace reconciliation
/// gate and the registry of Jobs it runs.
#[derive(Default)]
pub(super) struct Managed {
    initialized: tokio::sync::Mutex<HashSet<WorkspaceId>>,
    registry: Arc<Mutex<Registry>>,
}

fn lock(registry: &Mutex<Registry>) -> MutexGuard<'_, Registry> {
    registry.lock().expect("managed registry mutex poisoned")
}

/// A registry entry and its slot, owned by the Job's task. Dropped --
/// also on a failure or panic -- it leaves the registry, frees the slot,
/// then tells every waiter.
struct Registration {
    registry: Arc<Mutex<Registry>>,
    key: Key,
    control: Arc<Control>,
    slot: Option<Slot>,
    ended: Option<VerificationJobState>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        lock(&self.registry).jobs.remove(&self.key);
        drop(self.slot.take());
        self.control.done.send_replace(Done::Ended(self.ended));
    }
}

// ------------------------------------------------------------- the API

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ManagedError> + Send + 'static,
) -> Result<T, ManagedError> {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or(Err(ManagedError::Internal(
            "managed verification task failed",
        )))
}

fn workspace_db(root: &Path) -> PathBuf {
    WorkspacePaths::from_root(root).workspace_db
}

fn open(workspace: WorkspaceId, db: &Path) -> Result<VerificationJobStore, ManagedError> {
    VerificationJobStore::open_bound(workspace, db).map_err(storage)
}

impl DaemonQueryRuntime {
    /// The Workspace's root, after its one-time reconciliation: the first
    /// managed use in this daemon turns every RUNNING Job (an earlier
    /// daemon's) INTERRUPTED. Serialized, so no Job of this daemon exists
    /// before it; a failure leaves the Workspace unreconciled.
    async fn managed_workspace(&self, workspace: WorkspaceId) -> Result<PathBuf, ManagedError> {
        let root = self
            .workspace_root(workspace)
            .await
            .map_err(ManagedError::Workspace)?;
        let mut initialized = self.managed().initialized.lock().await;
        if !initialized.contains(&workspace) {
            let db = workspace_db(&root);
            blocking(move || {
                open(workspace, &db)?
                    .interrupt_all_running(&ended(EndReason::DaemonRestart))
                    .map_err(storage)
            })
            .await?;
            initialized.insert(workspace);
        }
        Ok(root)
    }

    /// #54: start (or replay) a managed verification Job. Returns as soon
    /// as the Job exists; its commands run in the background, whatever
    /// becomes of the caller.
    pub async fn managed_start(
        &self,
        workspace: WorkspaceId,
        idempotency_key: &str,
        verification: VerificationWire,
    ) -> Result<ManagedStart, ManagedError> {
        let root = self.managed_workspace(workspace).await?;
        let commands = verify::commands(verification);
        let db = workspace_db(&root);
        let prepared = {
            let commands = commands.clone();
            blocking(move || {
                verification::prepare(&root, &commands)
                    .map_err(|error| ManagedError::InvalidVerification(error.to_string()))
            })
            .await?
        };
        let key = IdempotencyKey::parse(idempotency_key)
            .map_err(|_| ManagedError::InvalidIdempotencyKey)?;
        let fingerprint = request_fingerprint(&commands);
        let existing = |key: IdempotencyKey, db: PathBuf| {
            blocking(move || {
                let store = open(workspace, &db)?;
                let Some(job) = store.get_by_idempotency_key(&key).map_err(storage)? else {
                    return Ok(None);
                };
                if job.request_fingerprint != fingerprint {
                    return Err(ManagedError::IdempotencyConflict);
                }
                Ok(Some(ManagedStart {
                    job_id: job.uid,
                    state: job.state,
                    replayed: true,
                    last_seq: store.last_seq(job.uid).map_err(storage)?,
                }))
            })
        };
        if let Some(replayed) = existing(key.clone(), db.clone()).await? {
            return Ok(replayed);
        }
        let Some(slot) = self.verification_slots().try_acquire(workspace) else {
            return Err(ManagedError::VerificationBusy);
        };

        // Registered before its row exists, so a shutdown that begins now
        // cannot miss it; no caller knows the ID yet.
        let job_id = VerificationJobId::generate();
        let control = Arc::new(Control {
            cancel: Cancel::default(),
            reason: Mutex::new(StopReason::None),
            done: watch::Sender::new(Done::Running),
        });
        let registration = {
            let registry = &self.managed().registry;
            let mut locked = lock(registry);
            if locked.closed {
                return Err(ManagedError::Internal("daemon is shutting down"));
            }
            locked
                .jobs
                .insert((workspace, job_id), Arc::clone(&control));
            Registration {
                registry: Arc::clone(registry),
                key: (workspace, job_id),
                control,
                slot: Some(slot),
                ended: None,
            }
        };
        // The row and its run start in one detached task: dropping this
        // future cannot leave a RUNNING row that nothing runs.
        let command_count = commands.len() as u8;
        let store = self.artifact_store();
        let (created, accepted) = tokio::sync::oneshot::channel();
        {
            let (key, db) = (key.clone(), db.clone());
            tokio::spawn(async move {
                let create = {
                    let db = db.clone();
                    blocking(move || {
                        let started = encode(&JobStartedPayload { v: V })
                            .map_err(|_| ManagedError::Internal("JOB_STARTED payload"))?;
                        match open(workspace, &db)?.create(&NewVerificationJob {
                            uid: job_id,
                            idempotency_key: &key,
                            request_fingerprint: fingerprint,
                            command_count,
                            started_payload_json: &started,
                        }) {
                            Ok(_) => Ok(true),
                            Err(VerificationJobError::IdempotencyKeyExists) => Ok(false),
                            Err(error) => Err(storage(error)),
                        }
                    })
                    .await
                };
                let run = matches!(create, Ok(true));
                // Not run: the registration and its slot go here.
                let registration = run.then_some(registration);
                let _ = created.send(create);
                if let Some(registration) = registration {
                    execute(registration, prepared, workspace, db, store).await;
                }
            });
        }
        match accepted.await.unwrap_or(Err(ManagedError::Internal(
            "managed verification task failed",
        )))? {
            true => {}
            // Lost a race for the key: the winner's Job is the answer.
            false => {
                return existing(key, db)
                    .await?
                    .ok_or(ManagedError::Internal("verification job vanished"));
            }
        }
        Ok(ManagedStart {
            job_id,
            state: VerificationJobState::Running,
            replayed: false,
            last_seq: 1,
        })
    }

    /// #54: up to `limit` (1..=64) events after `after_seq`. Reads only:
    /// nothing runs, no state changes.
    pub async fn managed_poll(
        &self,
        workspace: WorkspaceId,
        job_id: VerificationJobId,
        after_seq: u64,
        limit: usize,
    ) -> Result<ManagedPoll, ManagedError> {
        if !(1..=MAX_EVENTS_PER_READ).contains(&limit) {
            return Err(ManagedError::InvalidLimit);
        }
        let db = workspace_db(&self.managed_workspace(workspace).await?);
        let (rows, job, last_seq) = blocking(move || {
            let store = open(workspace, &db)?;
            let rows = store
                .events_after(job_id, after_seq, limit)
                .map_err(storage)?;
            let job = store
                .get_by_id(job_id)
                .map_err(storage)?
                .ok_or(ManagedError::JobNotFound)?;
            Ok((rows, job, store.last_seq(job_id).map_err(storage)?))
        })
        .await?;
        let store = self.artifact_store();
        let events = rows
            .iter()
            .map(|row| {
                let mut payload = decode(row)?;
                downgrade(&mut payload, &store);
                Ok(ManagedEvent {
                    seq: row.seq,
                    created_at: row.created_at.clone(),
                    payload,
                })
            })
            .collect::<Result<Vec<_>, ManagedError>>()?;
        let next_seq = events.last().map_or(after_seq, |event| event.seq);
        Ok(ManagedPoll {
            job,
            events,
            next_seq,
            has_more: next_seq < last_seq,
        })
    }

    /// #54: cancel a Job this daemon runs and return once its tree has
    /// ended and its terminal state is stored. A terminal Job is returned
    /// as it is.
    pub async fn managed_cancel(
        &self,
        workspace: WorkspaceId,
        job_id: VerificationJobId,
    ) -> Result<VerificationJobState, ManagedError> {
        let db = workspace_db(&self.managed_workspace(workspace).await?);
        let control = lock(&self.managed().registry)
            .jobs
            .get(&(workspace, job_id))
            .cloned();
        if let Some(control) = control {
            control.stop(StopReason::CallerCancelled);
            return control.ended().await.ok_or(ManagedError::Internal(
                "verification job could not be ended",
            ));
        }
        // Not running here: its terminal state was stored before it left
        // the registry.
        let job = blocking(move || open(workspace, &db)?.get_by_id(job_id).map_err(storage))
            .await?
            .ok_or(ManagedError::JobNotFound)?;
        match job.state {
            VerificationJobState::Running => Err(ManagedError::Internal(
                "verification job is not run by this daemon",
            )),
            state => Ok(state),
        }
    }

    /// #54 clean shutdown: no new Job; every running one is stopped
    /// (INTERRUPTED, not CANCELLED) and waited for until it is terminal.
    pub async fn managed_shutdown(&self) {
        let controls: Vec<Arc<Control>> = {
            let mut registry = lock(&self.managed().registry);
            registry.closed = true;
            registry.jobs.values().cloned().collect()
        };
        for control in &controls {
            control.stop(StopReason::DaemonShutdown);
        }
        for control in controls {
            control.ended().await;
        }
    }
}

// ------------------------------------------------------------- the run

/// The Job's background task: the run on the blocking pool (never the
/// async executor or the Workspace worker), then its registration goes.
async fn execute(
    mut registration: Registration,
    prepared: PreparedVerification,
    workspace: WorkspaceId,
    db: PathBuf,
    store: Arc<ArtifactStore>,
) {
    let job_id = registration.key.1;
    let control = Arc::clone(&registration.control);
    let run = {
        let db = db.clone();
        tokio::task::spawn_blocking(move || {
            run(&prepared, workspace, &db, job_id, &control, &store)
        })
    };
    registration.ended = match run.await {
        Ok(ended) => ended,
        Err(_) => {
            eprintln!("brainprintd: managed verification: run failed");
            tokio::task::spawn_blocking(move || {
                let mut jobs = VerificationJobStore::open_bound(workspace, &db).ok()?;
                terminalize(
                    &mut jobs,
                    job_id,
                    TerminalState::InternalError,
                    None,
                    &ended(EndReason::RunnerFailure),
                )
            })
            .await
            .ok()
            .flatten()
        }
    };
}

fn run(
    prepared: &PreparedVerification,
    workspace: WorkspaceId,
    db: &Path,
    job_id: VerificationJobId,
    control: &Control,
    store: &Arc<ArtifactStore>,
) -> Option<VerificationJobState> {
    let Ok(mut jobs) = VerificationJobStore::open_bound(workspace, db) else {
        eprintln!("brainprintd: managed verification: job storage unavailable");
        return None;
    };
    let labels = prepared.labels();
    let requested = prepared.capture_requested();
    let (ran, capture, failure) = {
        let mut observer = JobObserver {
            jobs: &mut jobs,
            job_id,
            capture: StoreConsumer::new(
                store,
                Undelivered::new(Arc::clone(store)),
                &labels,
                &requested,
            ),
            failure: EndReason::EventPersistence,
        };
        let ran = prepared.run_managed(&control.cancel, &mut observer);
        (ran, observer.capture, observer.failure)
    };
    let (to, summary, payload) = match ran {
        Ok(report) => {
            let mut results: Vec<CommandResultWire> = report
                .results
                .into_iter()
                .zip(capture::shape(capture.kept))
                .map(|(result, capture)| verify::result_wire(result, capture))
                .collect();
            capture::downgrade_evicted(&mut results, store);
            match encode(&JobFinishedPayload {
                v: V,
                verification_summary: report.summary.clone(),
                results,
            }) {
                Ok(payload) => (TerminalState::Finished, Some(report.summary), payload),
                Err(reason) => (TerminalState::InternalError, None, ended(reason)),
            }
        }
        Err(ManagedStop::Cancelled) => match *control.reason() {
            StopReason::CallerCancelled => (
                TerminalState::Cancelled,
                None,
                ended(EndReason::CallerCancelled),
            ),
            StopReason::DaemonShutdown | StopReason::None => (
                TerminalState::Interrupted,
                None,
                ended(EndReason::DaemonShutdown),
            ),
        },
        Err(ManagedStop::ObserverFailed) => (TerminalState::InternalError, None, ended(failure)),
    };
    // A failed command's fresh artifact (its event never stored) goes
    // with `capture.undelivered`; earlier ones are durable evidence.
    drop(capture.undelivered);
    terminalize(&mut jobs, job_id, to, summary.as_deref(), &payload).or_else(|| {
        // Best effort; still failing, the row stays RUNNING for the next
        // daemon's reconciliation.
        terminalize(
            &mut jobs,
            job_id,
            TerminalState::InternalError,
            None,
            &ended(EndReason::EventPersistence),
        )
    })
}

/// The stored terminal state, `to` or whichever came first.
fn terminalize(
    jobs: &mut VerificationJobStore,
    job_id: VerificationJobId,
    to: TerminalState,
    summary: Option<&str>,
    payload: &str,
) -> Option<VerificationJobState> {
    match jobs.finish(job_id, to, summary, payload) {
        Ok(Transition::AlreadyTerminal(state)) => Some(state),
        Ok(Transition::Applied { .. }) => Some(match to {
            TerminalState::Finished => VerificationJobState::Finished,
            TerminalState::Cancelled => VerificationJobState::Cancelled,
            TerminalState::Interrupted => VerificationJobState::Interrupted,
            TerminalState::InternalError => VerificationJobState::InternalError,
        }),
        Err(_) => {
            eprintln!("brainprintd: managed verification: terminal event not stored");
            None
        }
    }
}

/// The engine's lifecycle seam onto the Job's event log, with #53
/// capture through the shared [`StoreConsumer`].
struct JobObserver<'j, 'c> {
    jobs: &'j mut VerificationJobStore,
    job_id: VerificationJobId,
    capture: StoreConsumer<'c>,
    /// Why the last failed event failed.
    failure: EndReason,
}

impl JobObserver<'_, '_> {
    fn append(
        &mut self,
        event: ProgressEvent,
        payload: &impl Serialize,
    ) -> Result<(), ObserverFailed> {
        let json = encode(payload).map_err(|reason| {
            self.failure = reason;
            ObserverFailed
        })?;
        self.jobs.append(self.job_id, event, &json).map_err(|_| {
            eprintln!("brainprintd: managed verification: event not stored");
            self.failure = EndReason::EventPersistence;
            ObserverFailed
        })?;
        Ok(())
    }
}

impl ManagedObserver for JobObserver<'_, '_> {
    fn started(&mut self, index: usize, label: &str) -> Result<(), ObserverFailed> {
        self.append(
            ProgressEvent::CommandStarted,
            &CommandStartedPayload {
                v: V,
                index: index as u32,
                label: label.to_owned(),
            },
        )
    }

    fn capture(&mut self, index: usize, requested: CaptureRequest) -> CaptureRequest {
        self.capture.before(index, requested)
    }

    fn finished(
        &mut self,
        index: usize,
        result: &CommandResult,
        capture: Option<CommandCapture>,
    ) -> Result<(), ObserverFailed> {
        self.capture.after(index, capture);
        let payload = CommandFinishedPayload {
            v: V,
            index: index as u32,
            label: result.label.clone(),
            outcome: verify::outcome_wire(result.outcome),
            duration_ms: result.duration_ms,
            stdout_bytes: result.stdout_bytes,
            stderr_bytes: result.stderr_bytes,
            capture: progress(&self.capture.kept[index]),
        };
        self.append(ProgressEvent::CommandFinished, &payload)?;
        self.capture.undelivered.keep();
        Ok(())
    }
}

fn progress(kept: &Kept) -> CaptureProgress {
    match kept {
        Kept::NotRequested => CaptureProgress::NotRequested,
        Kept::NotRun => CaptureProgress::NotRun,
        Kept::Captured {
            stream_status,
            diagnostics,
            raw,
        } => CaptureProgress::Captured {
            stream_status: *stream_status,
            diagnostics: diagnostics.as_ref().map(|summary| DiagnosticCounts {
                observed: summary.observed,
                deduplicated: summary.deduplicated,
                omitted: summary.omitted,
                parse_misses: summary.parse_misses,
                retained: summary.items.len() as u64,
            }),
            raw: raw.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use brainprint_core::protocol::work::{RawArtifactRefWire, RawStreamMetaWire};
    use brainprint_engine::{
        diagnostics::{
            Diagnostic, DiagnosticPath, DiagnosticSummary, MAX_DIAGNOSTICS, MAX_MESSAGE_BYTES,
            Severity, Stream,
        },
        verification::{MAX_COMMANDS, MAX_LABEL_CHARS, MAX_SUMMARY_BYTES, VerificationOutcome},
    };

    use super::*;

    /// #53's largest terminal shape -- 16 commands × 64 maximal
    /// diagnostics, every raw reference available, maximal labels and
    /// summary -- encodes under the bound; the diagnostics budget is #53's.
    #[test]
    fn the_largest_job_finished_payload_fits() {
        let big = |tag: String| format!("{tag}{}", "x".repeat(MAX_MESSAGE_BYTES - tag.len()));
        let meta = RawStreamMetaWire {
            observed_bytes: u64::MAX,
            head_bytes: u64::MAX,
            tail_bytes: u64::MAX,
            omitted_bytes: u64::MAX,
            truncated: true,
            tail_start_offset: u64::MAX,
        };
        let kept: Vec<Kept> = (0..MAX_COMMANDS)
            .map(|command| Kept::Captured {
                stream_status: StreamStatusWire::Partial,
                diagnostics: Some(DiagnosticSummary {
                    items: (0..MAX_DIAGNOSTICS)
                        .map(|index| Diagnostic {
                            severity: Severity::Error,
                            code: Some(big(format!("code-{command}-{index}-"))),
                            message: big(format!("message-{command}-{index}-")),
                            path: DiagnosticPath::Workspace(big(format!(
                                "path-{command}-{index}-"
                            ))),
                            line: Some(u32::MAX),
                            column: Some(u32::MAX),
                            stream: Stream::Stderr,
                            message_truncated: true,
                        })
                        .collect(),
                    observed: u64::MAX,
                    deduplicated: u64::MAX,
                    omitted: u64::MAX,
                    parse_misses: u64::MAX,
                }),
                raw: RawAvailabilityWire::Available(RawArtifactRefWire {
                    handle: uuid::Uuid::new_v4().to_string(),
                    stdout: meta,
                    stderr: meta,
                }),
            })
            .collect();
        let results: Vec<CommandResultWire> = capture::shape(kept)
            .into_iter()
            .enumerate()
            .map(|(index, capture)| {
                let result = CommandResult {
                    label: format!("{index:0>MAX_LABEL_CHARS$}"),
                    outcome: VerificationOutcome::Failed {
                        exit_code: i32::MIN,
                    },
                    duration_ms: u64::MAX,
                    stdout_bytes: u64::MAX,
                    stderr_bytes: u64::MAX,
                };
                verify::result_wire(result, capture)
            })
            .collect();
        let payload = JobFinishedPayload {
            v: V,
            verification_summary: "s".repeat(MAX_SUMMARY_BYTES),
            results,
        };
        let json = encode(&payload).expect("fits");
        assert!(json.len() < MAX_PAYLOAD_BYTES, "{}", json.len());
        assert_eq!(MAX_PAYLOAD_BYTES, (1 << 20) - 4096);
    }

    #[test]
    fn an_oversized_payload_is_refused_not_stored() {
        let payload = JobFinishedPayload {
            v: V,
            verification_summary: "s".repeat(MAX_PAYLOAD_BYTES),
            results: Vec::new(),
        };
        assert_eq!(encode(&payload), Err(EndReason::EventPayload));
    }

    /// Every payload is `{"v":1,...}`; an unknown version or a payload of
    /// another kind's shape is corrupt, never guessed.
    #[test]
    fn payloads_are_versioned_and_decoded_by_kind() {
        let event = |kind, payload_json: &str| VerificationJobEvent {
            seq: 1,
            kind,
            payload_json: payload_json.to_owned(),
            created_at: String::new(),
        };
        assert_eq!(
            encode(&JobStartedPayload { v: V }).as_deref(),
            Ok(r#"{"v":1}"#)
        );
        assert_eq!(
            ended(EndReason::DaemonShutdown),
            r#"{"v":1,"reason":"daemon_shutdown"}"#
        );
        let started = encode(&CommandStartedPayload {
            v: V,
            index: 0,
            label: "test".to_owned(),
        })
        .expect("encode");
        assert_eq!(started, r#"{"v":1,"index":0,"label":"test"}"#);
        assert!(matches!(
            decode(&event(VerificationJobEventKind::CommandStarted, &started)),
            Ok(ManagedEventPayload::CommandStarted(_))
        ));
        for (kind, json) in [
            (
                VerificationJobEventKind::CommandStarted,
                r#"{"v":2,"index":0,"label":"t"}"#,
            ),
            (VerificationJobEventKind::JobStarted, &started),
            (VerificationJobEventKind::JobCancelled, r#"{"v":1}"#),
            (VerificationJobEventKind::JobStarted, "{}"),
            (VerificationJobEventKind::JobStarted, "not json"),
        ] {
            assert_eq!(
                decode(&event(kind, json)),
                Err(ManagedError::Corrupt),
                "{json}"
            );
        }
    }
}
