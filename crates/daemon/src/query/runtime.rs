//! Per-Workspace query runtime (#24 §14, §15) -- and, since #38, the one
//! product runtime of each active Workspace.
//!
//! One dedicated blocking thread per active Workspace, owning its
//! `CoreQuerySurface`, `DeliveryLedger`, pending-acknowledgement receipts,
//! recent ack tombstones, and its structural lifecycle
//! ([`super::lifecycle`]: watcher, event ingest, targeted refresh,
//! reconcile). Every client of a Workspace reaches the same thread, so
//! there is one watcher and one refresh/reconcile path per Workspace, and
//! publications are serialized with that Workspace's queries. The global map is locked only to look up or
//! create a worker -- never while a query runs -- so different Workspaces
//! execute concurrently and one Workspace's Task 8 ledger mutations are
//! naturally serialized by owning a single thread, without a global query
//! mutex or `unsafe impl Send/Sync`.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{Mutex, mpsc::RecvTimeoutError},
    time::Duration,
};

use brainprint_core::{
    WorkspaceId,
    protocol::query::{CorrelationWire, QueryErrorWire, QueryOperationWire, QueryResultWire},
};
use brainprint_engine::{
    paths::GlobalPaths,
    projection::planner::{DeliveryLedger, DeliveryReceipt, LedgerLimits},
    query_surface::{ContextPurpose, CoreQuerySurface},
};
use tokio::sync::oneshot;

use super::{
    convert_in, convert_out,
    lifecycle::{LifecycleStats, WatchFactory, WorkspaceLifecycle, notify_watch_factory},
    semantic::{SemanticStats, WorkspaceSemantic},
};

/// How often an idle worker journals buffered watcher events: the
/// engine's existing coalescing default (a tuning value, not a contract).
const WATCH_TICK: Duration =
    Duration::from_millis(brainprint_engine::watch::DEFAULT_COALESCE_WINDOW_MS);

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
    /// #38 init: bring the Workspace to structural READY.
    Activate {
        reply: oneshot::Sender<Result<(), String>>,
    },
    Stats {
        reply: oneshot::Sender<Option<LifecycleStats>>,
    },
    SemanticStats {
        reply: oneshot::Sender<Option<SemanticStats>>,
    },
}

#[derive(Clone)]
struct WorkerHandle {
    jobs: std::sync::mpsc::Sender<Job>,
}

/// The daemon's whole query runtime: a short-lived lock around worker
/// lookup/creation, never held during a query (#24 §14).
pub struct DaemonQueryRuntime {
    global_paths: GlobalPaths,
    global_db: PathBuf,
    watch_factory: WatchFactory,
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
    pub fn new(global_paths: &GlobalPaths) -> Self {
        Self::with_watch_factory(global_paths, notify_watch_factory())
    }

    /// Test seam: substitute the per-Workspace watch source (e.g. an
    /// unavailable or lossy watcher). Production uses [`Self::new`].
    #[must_use]
    pub fn with_watch_factory(global_paths: &GlobalPaths, watch_factory: WatchFactory) -> Self {
        Self {
            global_paths: global_paths.clone(),
            global_db: global_paths.global_db.clone(),
            watch_factory,
            workers: Mutex::new(HashMap::new()),
        }
    }

    /// How many Workspace runtimes this daemon owns.
    #[must_use]
    pub fn workspace_runtime_count(&self) -> usize {
        self.workers
            .lock()
            .expect("worker map mutex poisoned")
            .len()
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
        let global_paths = self.global_paths.clone();
        let factory = self.watch_factory.clone();
        std::thread::spawn(move || worker_loop(&global_paths, workspace, factory, &rx));
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

    /// #38: run the Workspace's activation (watcher, baseline or
    /// reconcile, settle) on its worker and wait for structural READY.
    pub async fn activate(&self, workspace: WorkspaceId) -> Result<(), String> {
        let handle = self.handle_for(workspace);
        let (reply, receiver) = oneshot::channel();
        if handle.jobs.send(Job::Activate { reply }).is_err() {
            return Err("workspace worker is gone".to_owned());
        }
        receiver
            .await
            .unwrap_or_else(|_| Err("workspace worker is gone".to_owned()))
    }

    /// The Workspace runtime's factual lifecycle counters, if it exists.
    pub async fn lifecycle_stats(&self, workspace: WorkspaceId) -> Option<LifecycleStats> {
        let handle = {
            let workers = self.workers.lock().expect("worker map mutex poisoned");
            workers.get(&workspace)?.clone()
        };
        let (reply, receiver) = oneshot::channel();
        handle.jobs.send(Job::Stats { reply }).ok()?;
        receiver.await.ok().flatten()
    }

    /// The Workspace's factual semantic runtime counters (#39), if its
    /// runtime exists and bound.
    pub async fn semantic_stats(&self, workspace: WorkspaceId) -> Option<SemanticStats> {
        let handle = {
            let workers = self.workers.lock().expect("worker map mutex poisoned");
            workers.get(&workspace)?.clone()
        };
        let (reply, receiver) = oneshot::channel();
        handle.jobs.send(Job::SemanticStats { reply }).ok()?;
        receiver.await.ok().flatten()
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

/// Whether an operation's answer depends on source/index currentness
/// (#38 §9). Knowledge (Policy/Decision/WorkItem/... facts) does not, and
/// is never held behind a code-index reconcile.
const fn requires_current(operation: &QueryOperationWire) -> bool {
    !matches!(operation, QueryOperationWire::Knowledge(_))
}

/// One Workspace's dedicated blocking worker thread: opens
/// `CoreQuerySurface` once, then serves jobs until the channel closes
/// (daemon lifetime; no P0 eviction, #24 §14). Between jobs it journals
/// buffered watcher events (#38).
fn worker_loop(
    global_paths: &GlobalPaths,
    workspace: WorkspaceId,
    factory: WatchFactory,
    jobs: &std::sync::mpsc::Receiver<Job>,
) {
    let global_db = global_paths.global_db.as_path();
    let (mut surface, mut lifecycle, mut semantic) = bind(global_paths, workspace, &factory);
    let mut ledger = new_ledger();
    let mut pending = PendingAcks::new();

    loop {
        let job = match jobs.recv_timeout(WATCH_TICK) {
            Ok(job) => job,
            Err(RecvTimeoutError::Timeout) => {
                if let Ok(lifecycle) = &mut lifecycle {
                    lifecycle.tick();
                }
                if let Some(semantic) = &mut semantic {
                    semantic.tick();
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match job {
            Job::Query {
                operation,
                correlation,
                reply,
            } => {
                if requires_current(&operation)
                    && let Ok(lifecycle) = &mut lifecycle
                    && let Err(error) = lifecycle.ensure_current()
                {
                    // Not fatal to the query: recovery failure leaves the
                    // index DIRTY, and the query reports NOT_CURRENT.
                    eprintln!(
                        "brainprintd: workspace {workspace} freshness recovery failed: {error}"
                    );
                }
                let outcome = match &surface {
                    Ok(surface) => run_query(
                        surface,
                        semantic.as_mut(),
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
            Job::Activate { reply } => {
                // `init` may have repaired the locator (directory move):
                // re-bind exactly as a fresh daemon process would.
                let stale = lifecycle.as_ref().map_or(true, |current| {
                    current.locator_changed(global_db, workspace)
                });
                if stale {
                    drop(lifecycle);
                    drop(semantic);
                    (surface, lifecycle, semantic) = bind(global_paths, workspace, &factory);
                    ledger = new_ledger();
                    pending = PendingAcks::new();
                }
                let outcome = match (&surface, &mut lifecycle) {
                    (Err(error), _) => {
                        Err(convert_out::core_error(clone_core_error(error)).message)
                    }
                    (Ok(_), Ok(lifecycle)) => lifecycle.activate(),
                    (Ok(_), Err(error)) => Err(error.clone()),
                };
                let _ = reply.send(outcome);
            }
            Job::Stats { reply } => {
                let _ = reply.send(lifecycle.as_ref().ok().map(WorkspaceLifecycle::stats));
            }
            Job::SemanticStats { reply } => {
                let _ = reply.send(semantic.as_ref().map(WorkspaceSemantic::stats));
            }
        }
    }
}

/// Open the Workspace's query surface and, only if its DB binding
/// validated, its lifecycle. The lifecycle writes index.db; a failed bind
/// (e.g. a binding mismatch) is reported by every query and never
/// "repaired" by a reconcile into foreign data files. The semantic
/// runtime (#39) is registered beside a bound lifecycle; registering
/// starts nothing.
fn bind(
    global_paths: &GlobalPaths,
    workspace: WorkspaceId,
    factory: &WatchFactory,
) -> (
    Result<CoreQuerySurface, brainprint_engine::query_surface::CoreError>,
    Result<WorkspaceLifecycle, String>,
    Option<WorkspaceSemantic>,
) {
    let global_db = global_paths.global_db.as_path();
    let surface = CoreQuerySurface::open(global_db, workspace);
    let lifecycle = match &surface {
        Ok(_) => WorkspaceLifecycle::load(global_db, workspace, factory.clone()),
        Err(_) => Err("workspace binding could not be validated".to_owned()),
    };
    let semantic = lifecycle.as_ref().ok().map(|lifecycle| {
        WorkspaceSemantic::load(
            global_paths,
            lifecycle.config(),
            workspace,
            lifecycle.root(),
            lifecycle.index_db(),
        )
    });
    (surface, lifecycle, semantic)
}

fn new_ledger() -> DeliveryLedger {
    DeliveryLedger::new(LedgerLimits::new(16, 1024).expect("16/1024 are non-zero ledger limits"))
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
    semantic: Option<&mut WorkspaceSemantic>,
    ledger: &mut DeliveryLedger,
    pending: &mut PendingAcks,
    workspace: WorkspaceId,
    operation_wire: QueryOperationWire,
    correlation: Option<CorrelationWire>,
) -> Result<(QueryResultWire, Option<String>), QueryErrorWire> {
    let tag = operation_tag(&operation_wire);
    let mut deliveries = Vec::new();
    let converted = convert_in::operation(operation_wire, workspace, correlation, &mut deliveries)?;

    // #39 lazy semantic demand: only the operations whose answer carries
    // direct relation/impact evidence for one selected target. Find,
    // knowledge and structure never reach a backend.
    if let Some(semantic) = semantic {
        let target = match &converted {
            convert_in::ConvertedOperation::Inspect(request) => Some(&request.target),
            convert_in::ConvertedOperation::Relations(request) => Some(&request.target),
            convert_in::ConvertedOperation::Impact(request) => Some(&request.target),
            convert_in::ConvertedOperation::Context(request) => match &request.purpose {
                ContextPurpose::Change { target, .. } => Some(target),
                ContextPurpose::Resume { .. } => None,
            },
            _ => None,
        };
        if let Some(target) = target {
            semantic.enrich(surface, target);
        }
    }

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
