//! Per-Workspace query runtime (#24 §14, §15).
//!
//! One dedicated blocking thread per active Workspace, owning its
//! `CoreQuerySurface`, `DeliveryLedger`, pending-acknowledgement receipts
//! and recent ack tombstones. The global map is locked only to look up or
//! create a worker -- never while a query runs -- so different Workspaces
//! execute concurrently and one Workspace's Task 8 ledger mutations are
//! naturally serialized by owning a single thread, without a global query
//! mutex or `unsafe impl Send/Sync`.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Mutex,
};

use brainprint_core::{
    WorkspaceId,
    protocol::query::{CorrelationWire, QueryErrorWire, QueryOperationWire, QueryResultWire},
};
use brainprint_engine::{
    projection::planner::{DeliveryLedger, DeliveryReceipt, LedgerLimits},
    query_surface::CoreQuerySurface,
};
use tokio::sync::oneshot;

use super::{convert_in, convert_out};

/// #24 §15: bounded FIFO state, separate from the Task 8 ledger itself.
const PENDING_ACK_BOUND: usize = 256;
const TOMBSTONE_BOUND: usize = 256;

/// #24 §24: reserved headroom in the 1 MiB frame for the envelope fields
/// (`request_id`, `workspace_id`, `ack_token`, JSON structure) that wrap
/// a `QueryResultWire`. The preflight check runs against the result alone
/// so a query never mints/stores a pending receipt for a page it is about
/// to refuse to send.
const ENVELOPE_HEADROOM_BYTES: usize = 4096;
const MAX_RESULT_BYTES: usize =
    brainprint_core::protocol::framing::MAX_MESSAGE_BYTES as usize - ENVELOPE_HEADROOM_BYTES;

pub enum AckOutcome {
    Acknowledged,
    AlreadyAcknowledged,
    UnknownOrExpired,
}

// One job per RPC call, sent once through an unbounded channel; never a
// hot loop, so boxing purely to shrink this enum's stack footprint would
// not be a measurable win.
#[allow(clippy::large_enum_variant)]
enum Job {
    Query {
        operation: QueryOperationWire,
        correlation: Option<CorrelationWire>,
        reply: oneshot::Sender<Result<(QueryResultWire, Option<String>), QueryErrorWire>>,
    },
    Ack {
        ack_token: String,
        reply: oneshot::Sender<AckOutcome>,
    },
}

#[derive(Clone)]
struct WorkerHandle {
    jobs: std::sync::mpsc::Sender<Job>,
}

/// The daemon's whole query runtime: a short-lived lock around worker
/// lookup/creation, never held during a query (#24 §14).
pub struct DaemonQueryRuntime {
    global_db: PathBuf,
    workers: Mutex<HashMap<WorkspaceId, WorkerHandle>>,
}

impl std::fmt::Debug for DaemonQueryRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DaemonQueryRuntime")
            .field("global_db", &self.global_db)
            .finish_non_exhaustive()
    }
}

impl DaemonQueryRuntime {
    #[must_use]
    pub fn new(global_db: PathBuf) -> Self {
        Self {
            global_db,
            workers: Mutex::new(HashMap::new()),
        }
    }

    /// The one Workspace registered at `locator`, resolved on a blocking
    /// thread (#24 §14: locator resolution happens before worker
    /// lookup/creation, never inside the runtime lock).
    // `CoreError` (engine's own Task 10 error type, out of Task 11's
    // scope to restructure) is the closure's `Err`; this runs once per
    // Query call, never a hot loop.
    #[allow(clippy::result_large_err)]
    pub async fn resolve_workspace(&self, locator: PathBuf) -> Result<WorkspaceId, QueryErrorWire> {
        let global_db = self.global_db.clone();
        match tokio::task::spawn_blocking(move || {
            CoreQuerySurface::resolve_workspace(&global_db, &locator)
        })
        .await
        {
            Ok(result) => result.map_err(convert_out::core_error),
            Err(error) => Err(convert_out::daemon_internal("resolve_workspace", &error)),
        }
    }

    fn handle_for(&self, workspace: WorkspaceId) -> WorkerHandle {
        let mut workers = self.workers.lock().expect("worker map mutex poisoned");
        if let Some(handle) = workers.get(&workspace) {
            return handle.clone();
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let global_db = self.global_db.clone();
        std::thread::spawn(move || worker_loop(&global_db, workspace, &rx));
        let handle = WorkerHandle { jobs: tx };
        workers.insert(workspace, handle.clone());
        handle
    }

    /// Dispatch one query onto `workspace`'s dedicated worker.
    pub async fn query(
        &self,
        workspace: WorkspaceId,
        operation: QueryOperationWire,
        correlation: Option<CorrelationWire>,
    ) -> Result<(QueryResultWire, Option<String>), QueryErrorWire> {
        let handle = self.handle_for(workspace);
        let (reply, receiver) = oneshot::channel();
        if handle
            .jobs
            .send(Job::Query {
                operation,
                correlation,
                reply,
            })
            .is_err()
        {
            return Err(worker_gone());
        }
        receiver.await.unwrap_or_else(|_| Err(worker_gone()))
    }

    /// Acknowledge a pending receipt on `workspace`'s worker.
    pub async fn ack(&self, workspace: WorkspaceId, ack_token: String) -> AckOutcome {
        let handle = self.handle_for(workspace);
        let (reply, receiver) = oneshot::channel();
        if handle.jobs.send(Job::Ack { ack_token, reply }).is_err() {
            return AckOutcome::UnknownOrExpired;
        }
        receiver.await.unwrap_or(AckOutcome::UnknownOrExpired)
    }
}

fn worker_gone() -> QueryErrorWire {
    convert_out::daemon_internal("worker", &std::io::Error::other("workspace worker is gone"))
}

// ------------------------------------------------------------- worker

struct PendingAcks {
    receipts: HashMap<String, DeliveryReceipt>,
    order: VecDeque<String>,
    tombstones: HashSet<String>,
    tombstone_order: VecDeque<String>,
}

impl PendingAcks {
    fn new() -> Self {
        Self {
            receipts: HashMap::new(),
            order: VecDeque::new(),
            tombstones: HashSet::new(),
            tombstone_order: VecDeque::new(),
        }
    }

    fn insert(&mut self, token: String, receipt: DeliveryReceipt) {
        if self.order.len() >= PENDING_ACK_BOUND
            && let Some(oldest) = self.order.pop_front()
        {
            self.receipts.remove(&oldest);
        }
        self.order.push_back(token.clone());
        self.receipts.insert(token, receipt);
    }

    fn tombstone(&mut self, token: String) {
        if self.tombstone_order.len() >= TOMBSTONE_BOUND
            && let Some(oldest) = self.tombstone_order.pop_front()
        {
            self.tombstones.remove(&oldest);
        }
        self.tombstone_order.push_back(token.clone());
        self.tombstones.insert(token);
    }

    /// #24 §15's ack state machine: pending -> acknowledge + tombstone;
    /// tombstoned -> `AlreadyAcknowledged` without a second acknowledge;
    /// unknown/evicted -> `UnknownOrExpired` without touching the ledger.
    fn ack(&mut self, token: &str, ledger: &mut DeliveryLedger) -> AckOutcome {
        if let Some(receipt) = self.receipts.remove(token) {
            self.order.retain(|held| held != token);
            let _ = ledger.acknowledge(&receipt);
            self.tombstone(token.to_owned());
            AckOutcome::Acknowledged
        } else if self.tombstones.contains(token) {
            AckOutcome::AlreadyAcknowledged
        } else {
            AckOutcome::UnknownOrExpired
        }
    }
}

fn operation_tag(operation: &QueryOperationWire) -> &'static str {
    match operation {
        QueryOperationWire::Find(_) => "find",
        QueryOperationWire::Inspect(_) => "inspect",
        QueryOperationWire::Relations(_) => "relations",
        QueryOperationWire::Impact(_) => "impact",
        QueryOperationWire::Context(_) => "context",
        QueryOperationWire::Knowledge(_) => "knowledge",
        QueryOperationWire::Structure(_) => "structure",
    }
}

/// One Workspace's dedicated blocking worker thread: opens
/// `CoreQuerySurface` once, then serves jobs until the channel closes
/// (daemon lifetime; no P0 eviction, #24 §14).
fn worker_loop(global_db: &Path, workspace: WorkspaceId, jobs: &std::sync::mpsc::Receiver<Job>) {
    let surface = CoreQuerySurface::open(global_db, workspace);
    let mut ledger = DeliveryLedger::new(
        LedgerLimits::new(16, 1024).expect("16/1024 are non-zero ledger limits"),
    );
    let mut pending = PendingAcks::new();

    while let Ok(job) = jobs.recv() {
        match job {
            Job::Query {
                operation,
                correlation,
                reply,
            } => {
                let outcome = match &surface {
                    Ok(surface) => run_query(
                        surface,
                        &mut ledger,
                        &mut pending,
                        workspace,
                        operation,
                        correlation,
                    ),
                    Err(error) => Err(convert_out::core_error(clone_core_error(error))),
                };
                let _ = reply.send(outcome);
            }
            Job::Ack { ack_token, reply } => {
                let outcome = pending.ack(&ack_token, &mut ledger);
                let _ = reply.send(outcome);
            }
        }
    }
}

/// `CoreError` is not `Clone`; every worker-startup failure path needs its
/// own wire mapping, so this rebuilds an equivalent error from the parts
/// that matter to a client rather than storing/cloning the original.
fn clone_core_error(
    error: &brainprint_engine::query_surface::CoreError,
) -> brainprint_engine::query_surface::CoreError {
    use brainprint_engine::query_surface::{CoreError, NotInitialized};
    match error {
        CoreError::NotInitialized(reason) => CoreError::NotInitialized(*reason),
        CoreError::WorkspaceRootMissing { workspace } => CoreError::WorkspaceRootMissing {
            workspace: *workspace,
        },
        CoreError::WorkspaceLocatorAmbiguous { workspaces } => {
            CoreError::WorkspaceLocatorAmbiguous {
                workspaces: workspaces.clone(),
            }
        }
        CoreError::WorkspaceMismatch { bound, requested } => CoreError::WorkspaceMismatch {
            bound: *bound,
            requested: *requested,
        },
        CoreError::WorkspaceBindingMismatch {
            db,
            expected,
            found,
        } => CoreError::WorkspaceBindingMismatch {
            db,
            expected: *expected,
            found: *found,
        },
        // Any other open() failure is a storage/registry-layer detail:
        // safe as a generic NotInitialized(WorkspaceNotRegistered), since
        // the Workspace was just resolved successfully moments earlier by
        // the same registry and open() re-reads the same rows.
        _ => CoreError::NotInitialized(NotInitialized::WorkspaceNotRegistered),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_query(
    surface: &CoreQuerySurface,
    ledger: &mut DeliveryLedger,
    pending: &mut PendingAcks,
    workspace: WorkspaceId,
    operation_wire: QueryOperationWire,
    correlation: Option<CorrelationWire>,
) -> Result<(QueryResultWire, Option<String>), QueryErrorWire> {
    let tag = operation_tag(&operation_wire);
    let mut deliveries = Vec::new();
    let converted = convert_in::operation(operation_wire, workspace, correlation, &mut deliveries)?;

    let (result_wire, receipt): (QueryResultWire, Option<DeliveryReceipt>) = match converted {
        convert_in::ConvertedOperation::Find(request) => {
            let result = surface
                .find(request, ledger)
                .map_err(convert_out::core_error)?;
            let (wire, receipt) = convert_out::find_result_wire(result);
            (QueryResultWire::Find(wire), receipt)
        }
        convert_in::ConvertedOperation::Inspect(request) => {
            let answer = surface
                .inspect(request, ledger)
                .map_err(convert_out::core_error)?;
            let (wire, receipt) = convert_out::projected_answer_wire(answer);
            (QueryResultWire::Inspect(wire), Some(receipt))
        }
        convert_in::ConvertedOperation::Relations(request) => {
            let result = surface
                .relations(request)
                .map_err(convert_out::core_error)?;
            (
                QueryResultWire::Relations(convert_out::relations_result_wire(result)),
                None,
            )
        }
        convert_in::ConvertedOperation::Impact(request) => {
            let answer = surface
                .impact(request, ledger)
                .map_err(convert_out::core_error)?;
            let (wire, receipt) = convert_out::projected_answer_wire(answer);
            (QueryResultWire::Impact(wire), Some(receipt))
        }
        convert_in::ConvertedOperation::Context(request) => {
            let answer = surface
                .context(request, ledger)
                .map_err(convert_out::core_error)?;
            let (wire, receipt) = convert_out::projected_answer_wire(answer);
            (QueryResultWire::Context(wire), Some(receipt))
        }
        convert_in::ConvertedOperation::Knowledge(request) => {
            let result = surface
                .knowledge(request)
                .map_err(convert_out::core_error)?;
            (
                QueryResultWire::Knowledge(convert_out::knowledge_result_wire(result)),
                None,
            )
        }
        convert_in::ConvertedOperation::Structure(request) => {
            let summary = surface
                .structure(&request)
                .map_err(convert_out::core_error)?;
            (
                QueryResultWire::Structure(convert_out::structural_summary_wire(summary)),
                None,
            )
        }
    };

    let encoded_bytes = serde_json::to_vec(&result_wire)
        .map_err(|error| convert_out::daemon_internal("encode", &error))?
        .len();
    if encoded_bytes > MAX_RESULT_BYTES {
        // #24 §10: never truncate, never split, never send a partial
        // result -- and never mint/store a pending receipt for a page
        // that is about to be refused.
        return Err(convert_out::result_too_large(
            tag,
            encoded_bytes,
            MAX_RESULT_BYTES,
        ));
    }

    let ack_token = receipt.map(|receipt| {
        let token = uuid::Uuid::new_v4().to_string();
        pending.insert(token.clone(), receipt);
        token
    });
    Ok((result_wire, ack_token))
}
