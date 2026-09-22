//! The shared semantic runtime supervisor (#19 task 2).
//!
//! Task 1 said what a semantic analysis *is*: an [`AnalysisContext`],
//! and capabilities and evidence spoken in Brainprint identity. This
//! module owns the thing that answers: one warm runtime per context,
//! shared by everyone who asks for that context, started when the first
//! question arrives and not before.
//!
//! ## Who owns a backend
//!
//! Not an Agent, a session, an MCP client, a request, or a conversation.
//! A runtime is keyed by [`AnalysisContext::context_key`], and that key
//! has no client identity in it, so Claude, Codex and Gemini looking at
//! the same Workspace, language, project root and toolchain reach the
//! same runtime. A different worktree has a different `WorkspaceId`, so
//! it is a different key and shares nothing current.
//!
//! The consequence that matters: **an Agent going away is not a
//! shutdown**. Leases keep a runtime alive while work needs it; idle
//! policy retires it when nothing does.
//!
//! ## Logical runtime, and the host under it
//!
//! A [`SemanticRuntimeHost`] is what actually answers -- today a child
//! process, a thread, or a test fake. One logical context maps to one
//! host here, but nothing in the contract *requires* that forever: a
//! [`SemanticBackendLauncher`] hands back an `Arc`, and a launcher that
//! later wants one host to serve several contexts can return the same
//! one without any change on this side. That is why the two concepts
//! are separate types rather than one.
//!
//! No child-process handle, file descriptor, or raw backend output
//! crosses this boundary. A host owns those; the supervisor sees
//! [`HostHealth`], a payload, and errors.
//!
//! ## What this module does not do
//!
//! It runs no SQL, opens no `index.db`, and persists nothing. Semantic
//! freshness, analysis-profile persistence and stable publication are
//! task 3; merging evidence into the graph is task 4. A runtime being
//! READY says nothing about whether the semantic index is current, and
//! a runtime being STOPPED does not make a published index disappear --
//! the two states are deliberately not the same state machine.
//!
//! I2/I3 never construct a [`SemanticRuntimeSupervisor`]. Structural
//! truth does not depend on a semantic backend existing.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    error::Error,
    fmt,
    panic::{self, AssertUnwindSafe},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    db,
    semantic::{AnalysisContext, AnalysisContextBinding, SemanticBackendKind, SemanticCapability},
};

/// How many latency samples one runtime keeps. Bounded on purpose: this
/// is a telemetry foundation, not a time series database.
const LATENCY_SAMPLE_LIMIT: usize = 64;

// ---------------------------------------------------------------------
// Backend abstraction
// ---------------------------------------------------------------------

/// Whether a host is still able to answer.
///
/// Deliberately not a process exit code, a signal, or an LSP status: a
/// host that lives in a child process maps whatever it knows onto this,
/// and nothing above this line learns how it is implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostHealth {
    Healthy,
    /// The backend is gone -- exited, unreachable, or wedged past
    /// recovery. The supervisor retires the host and may restart it
    /// under the backoff policy.
    Crashed,
}

/// Whether a host can run more than one request at a time.
///
/// Explicit rather than assumed: plenty of language services are
/// single-threaded, and pretending otherwise corrupts results. A
/// [`Self::Serial`] host gets a priority-ordered gate; a
/// [`Self::Parallel`] one does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostConcurrency {
    Serial,
    Parallel,
}

/// Process-level resource observation, when it is available.
///
/// Both fields are `Option` because neither is portably observable from
/// safe Rust in this workspace (`unsafe_code` is denied), and a zero
/// would be a fabricated measurement. Unavailable says unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResourceUsage {
    pub rss_bytes: Option<u64>,
    pub cpu_millis: Option<u64>,
}

impl ResourceUsage {
    /// Nothing measured. The honest default.
    pub const UNAVAILABLE: Self = Self {
        rss_bytes: None,
        cpu_millis: None,
    };
}

/// A live semantic backend runtime.
///
/// Implementations own whatever the backend really is -- a child
/// process, a connection, a fake. Nothing here names a protocol: no
/// Pyright request, no LSP method, no tsserver project handle.
pub trait SemanticRuntimeHost: Send + Sync {
    /// Run one request. `cancel` is cooperative: a host that can stop
    /// early should check it, and a host that cannot may ignore it.
    fn execute(
        &self,
        request: &RuntimeRequest,
        cancel: &CancelToken,
    ) -> Result<RuntimeResponse, HostError>;

    /// Whether the backend is still usable. Checked after every
    /// request, so a crash is noticed without polling.
    fn health(&self) -> HostHealth;

    /// Release everything the host owns. Called once, by the
    /// supervisor, on idle unload, crash retirement, or shutdown.
    fn shutdown(&self);

    /// Whether requests may overlap. Defaults to the safe answer.
    fn concurrency(&self) -> HostConcurrency {
        HostConcurrency::Serial
    }

    /// Process resource observation, when the host can measure it.
    fn resource_usage(&self) -> ResourceUsage {
        ResourceUsage::UNAVAILABLE
    }
}

/// Starts hosts for one backend family.
///
/// Takes the [`AnalysisContextBinding`], not just the context: starting
/// a process needs the *current* locator, which task 1 deliberately kept
/// out of the identity.
pub trait SemanticBackendLauncher: Send + Sync {
    fn kind(&self) -> SemanticBackendKind;

    fn launch(
        &self,
        binding: &AnalysisContextBinding,
    ) -> Result<Arc<dyn SemanticRuntimeHost>, HostError>;
}

/// What a backend said went wrong, as text the supervisor only ever
/// reports -- never parses, and never turns into semantic evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    pub message: String,
}

impl HostError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for HostError {}

// ---------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------

/// Whether a request is something a human is waiting on.
///
/// Two values, not a scheduler. Ordered so [`Self::Interactive`] sorts
/// first, which is the whole mechanism: a serial host's queue is a
/// sorted set, so interactive work is never structurally stuck behind a
/// background backlog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RequestPriority {
    Interactive,
    Background,
}

/// The identity of one semantic question.
///
/// Identity, not a prompt: `target` is a normalized target the caller
/// already resolved (a qualified name, a Resource path key, a span), so
/// two callers asking the same question produce the same key. Free text
/// must never be hashed into this -- it would make every phrasing a
/// different question and dedupe nothing.
///
/// `basis_token` is the hook task 3 fills in with source/config
/// revision, so that results from a superseded basis never coalesce
/// with current ones. Task 2 leaves it to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticRequestKey {
    pub context_key: String,
    pub capability: SemanticCapability,
    pub target: String,
    pub basis_token: Option<String>,
}

impl SemanticRequestKey {
    /// The deterministic key two identical in-flight requests share.
    #[must_use]
    pub fn dedupe_key(&self) -> String {
        db::fingerprint(
            "semantic-request-1",
            &[
                ("context", &self.context_key),
                ("capability", self.capability.as_str()),
                ("target", &self.target),
                ("basis", self.basis_token.as_deref().unwrap_or("")),
                // An absent basis is not an empty one.
                (
                    "basis_present",
                    if self.basis_token.is_some() { "1" } else { "0" },
                ),
            ],
        )
    }
}

/// One request as the runtime carries it.
///
/// `payload` is backend-neutral bytes an adapter understands. It is
/// transport, never truth: what becomes canonical is
/// [`crate::semantic::SemanticEvidence`] that an adapter normalizes out
/// of the response, and nothing in this module writes either anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRequest {
    pub key: SemanticRequestKey,
    pub priority: RequestPriority,
    pub payload: Vec<u8>,
}

/// One backend answer, still in backend-neutral form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeResponse {
    pub payload: Vec<u8>,
}

/// How a request is bounded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestOptions {
    /// `None` waits indefinitely. A caller that means "not forever"
    /// says so.
    pub timeout: Option<Duration>,
}

impl RequestOptions {
    #[must_use]
    pub const fn with_timeout(timeout: Duration) -> Self {
        Self {
            timeout: Some(timeout),
        }
    }
}

/// A cooperative cancellation flag shared by a request and its host.
///
/// Generation-neutral on purpose: task 3 marks a request obsolete by
/// cancelling it, and needs nothing from this type but the ability to
/// set the flag.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

/// Runtime control identity for one in-flight execution.
///
/// Crate-private by construction, which is the point: a request id
/// exists for cancellation, timeout and telemetry, and there is no way
/// for it to leak into a Resource, Symbol, Relation, or any
/// [`crate::semantic::SemanticEvidence`], because no public type
/// carries one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequestId(u64);

// ---------------------------------------------------------------------
// Failures
// ---------------------------------------------------------------------

/// Why a runtime could not be acquired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartFailure {
    /// Nothing is registered to serve this backend family.
    NoBackendRegistered(SemanticBackendKind),
    /// The launcher failed.
    Backend(HostError),
    /// The runtime crashed recently and the backoff window has not
    /// elapsed. Not a permanent state, and not a restart either.
    InBackoff {
        attempts: u32,
        retry_after: Duration,
    },
    /// The restart budget is spent. Degraded until something changes;
    /// callers fall back to structural answers.
    Degraded { attempts: u32 },
    /// Every live runtime slot is taken and none of them may be
    /// retired, because each is leased or running a request.
    ///
    /// Not [`Self::Backend`] and not an empty answer: the backend is
    /// fine, the fleet is full. A caller falls back to persisted or
    /// structural truth and tries again later.
    Capacity { limit: usize, live: usize },
    /// The supervisor is shutting down and takes no new work.
    ShuttingDown,
}

impl fmt::Display for StartFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoBackendRegistered(kind) => {
                write!(formatter, "no semantic backend registered for {kind}")
            }
            Self::Backend(source) => write!(formatter, "semantic backend start failed: {source}"),
            Self::InBackoff {
                attempts,
                retry_after,
            } => write!(
                formatter,
                "semantic runtime in backoff after {attempts} failed starts, retry in {retry_after:?}"
            ),
            Self::Degraded { attempts } => write!(
                formatter,
                "semantic runtime degraded after {attempts} failed starts"
            ),
            Self::Capacity { limit, live } => write!(
                formatter,
                "semantic runtime capacity is full: {live} live, limit {limit}, none retirable"
            ),
            Self::ShuttingDown => formatter.write_str("semantic supervisor is shutting down"),
        }
    }
}

impl Error for StartFailure {}

/// Why a request did not produce an answer.
///
/// Every variant is an error. None of them is an empty success: a
/// timeout, a cancellation, or a crash must never reach a caller as
/// "zero semantic results".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestFailure {
    /// The runtime was not READY when the request was submitted.
    NotReady(RuntimeState),
    TimedOut {
        after: Duration,
    },
    Cancelled,
    /// The backend died while running this request.
    Crashed(HostError),
    /// The backend answered with a failure. The runtime is still fine.
    Backend(HostError),
    ShuttingDown,
}

impl fmt::Display for RequestFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotReady(state) => write!(formatter, "semantic runtime is {state}, not ready"),
            Self::TimedOut { after } => {
                write!(formatter, "semantic request timed out after {after:?}")
            }
            Self::Cancelled => formatter.write_str("semantic request was cancelled"),
            Self::Crashed(source) => write!(formatter, "semantic backend crashed: {source}"),
            Self::Backend(source) => write!(formatter, "semantic backend failed: {source}"),
            Self::ShuttingDown => formatter.write_str("semantic supervisor is shutting down"),
        }
    }
}

impl Error for RequestFailure {}

// ---------------------------------------------------------------------
// Runtime state and policy
// ---------------------------------------------------------------------

/// What a logical runtime is doing.
///
/// Runtime lifecycle only. It says nothing about whether the semantic
/// index is current -- that is task 3's freshness, and a READY runtime
/// with a DIRTY index is a normal, expressible state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeState {
    /// Never started, or unloaded and not yet needed again.
    Cold,
    Starting,
    /// Warm, with no request running and not yet idle-eligible.
    Ready,
    /// At least one request is running.
    Busy,
    /// Warm, unused, and past the idle policy: eligible for unload, and
    /// still instantly reusable until it is.
    Idle,
    /// Crashed or failed to start; a restart is allowed once the
    /// backoff window elapses.
    Backoff,
    /// The restart budget is spent.
    Degraded,
    /// Explicitly stopped -- idle unload or supervisor shutdown.
    Stopped,
}

impl fmt::Display for RuntimeState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Cold => "COLD",
            Self::Starting => "STARTING",
            Self::Ready => "READY",
            Self::Busy => "BUSY",
            Self::Idle => "IDLE",
            Self::Backoff => "BACKOFF",
            Self::Degraded => "DEGRADED",
            Self::Stopped => "STOPPED",
        })
    }
}

/// The deterministic lifecycle rules. Every value is configurable, and
/// none of them is a product decision: the defaults are here so tests
/// and early callers have something, not because they were benchmarked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimePolicy {
    /// How long a warm, unused runtime stays before
    /// [`SemanticRuntimeSupervisor::sweep_idle`] may unload it.
    pub idle_timeout: Duration,
    /// How many consecutive failures before the runtime is DEGRADED
    /// rather than retried.
    pub restart_budget: u32,
    /// First backoff window. Doubles per consecutive failure, capped at
    /// [`Self::backoff_max`].
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// How many runtimes may hold a live host at once, across every
    /// backend family.
    ///
    /// `None` is unlimited, and is the default on purpose: what the
    /// number should be is a measurement nobody has taken yet (#19
    /// task 15), and inventing one here would be a product decision
    /// dressed up as a constant. The mechanism exists so that a
    /// deployment which needs a bound can state one; the bound itself
    /// is not this task's to choose.
    ///
    /// The cap is supervisor-wide, not per family. Five Python
    /// contexts and one Rust context are six live runtimes, and no
    /// language is privileged over another.
    pub max_live_runtimes: Option<usize>,
}

impl Default for RuntimePolicy {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(300),
            restart_budget: 3,
            backoff_base: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
            max_live_runtimes: None,
        }
    }
}

impl RuntimePolicy {
    /// The backoff window after `attempts` consecutive failures.
    ///
    /// Deterministic doubling, capped -- not adaptive, not jittered,
    /// and not tuned. `attempts` is 1-based.
    #[must_use]
    pub fn backoff_for(&self, attempts: u32) -> Duration {
        if attempts == 0 {
            return Duration::ZERO;
        }
        let shift = attempts.saturating_sub(1).min(31);
        self.backoff_base
            .saturating_mul(1_u32 << shift)
            .min(self.backoff_max)
    }
}

// ---------------------------------------------------------------------
// Telemetry
// ---------------------------------------------------------------------

/// One runtime's counters, as a snapshot.
///
/// Cheap enough to keep always on, and the foundation later resource
/// governance and benchmarks read. Nothing here is fabricated: an
/// unmeasurable field is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeTelemetry {
    pub state: RuntimeState,
    pub starts_attempted: u64,
    pub starts_succeeded: u64,
    /// Starts that followed a failure -- restart storms show up here.
    pub restart_attempts: u64,
    pub crashes: u64,
    pub consecutive_failures: u32,
    pub requests_started: u64,
    pub requests_completed: u64,
    pub requests_cancelled: u64,
    pub requests_timed_out: u64,
    pub requests_failed: u64,
    /// Requests that joined an identical in-flight execution instead of
    /// starting a second one.
    pub dedupe_hits: u64,
    pub active_requests: usize,
    pub active_leases: usize,
    pub queue_depth: usize,
    /// Since the last acquire, request start, or request completion.
    pub idle_for: Duration,
    /// Bounded, most recent last.
    pub latency_samples: Vec<Duration>,
    pub resource_usage: ResourceUsage,
}

/// Every runtime the supervisor knows about, added up.
///
/// Operational state, never project knowledge: nothing here is
/// persisted, nothing here is evidence, and reading it starts no
/// backend -- a context that has never been acquired contributes
/// nothing because it has no entry to contribute.
///
/// The resource fields are the reason this is a struct rather than a
/// number. A fleet where one host reports 100 MB and another reports
/// nothing has a *known* total of 100 MB and one unknown host, and
/// saying "100 MB" flat would be a claim about memory nobody measured.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FleetTelemetry {
    /// Contexts with a runtime entry, whatever state it is in.
    pub known_contexts: usize,
    /// Entries currently holding a host.
    pub live_runtimes: usize,

    pub cold: usize,
    pub starting: usize,
    pub ready: usize,
    pub busy: usize,
    pub idle: usize,
    pub backoff: usize,
    pub degraded: usize,
    pub stopped: usize,

    pub active_requests: usize,
    pub active_leases: usize,
    pub queue_depth: usize,

    pub starts_attempted: u64,
    pub starts_succeeded: u64,
    pub restart_attempts: u64,
    pub crashes: u64,
    /// Runtimes retired to make room under the capacity policy.
    pub capacity_evictions: u64,
    pub requests_started: u64,
    pub requests_completed: u64,
    pub requests_cancelled: u64,
    pub requests_timed_out: u64,
    pub requests_failed: u64,
    pub dedupe_hits: u64,

    /// The sum over live hosts that could measure it, or `None` when
    /// not one of them could. Never a fabricated zero.
    pub known_rss_bytes: Option<u64>,
    pub known_cpu_millis: Option<u64>,
    /// Live hosts whose resident size is not observable.
    pub resource_usage_unknown: usize,
}

#[derive(Debug, Default)]
struct Counters {
    starts_attempted: u64,
    starts_succeeded: u64,
    restart_attempts: u64,
    crashes: u64,
    requests_started: u64,
    requests_completed: u64,
    requests_cancelled: u64,
    requests_timed_out: u64,
    requests_failed: u64,
    dedupe_hits: u64,
    latency: VecDeque<Duration>,
}

impl Counters {
    fn sample(&mut self, latency: Duration) {
        if self.latency.len() == LATENCY_SAMPLE_LIMIT {
            self.latency.pop_front();
        }
        self.latency.push_back(latency);
    }
}

// ---------------------------------------------------------------------
// Internal runtime entry
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Cold,
    Starting,
    Ready,
    Backoff,
    Degraded,
    Stopped,
}

/// One queued request on a serial host, ordered by priority then
/// arrival. Interactive sorts first, which is all the scheduling this
/// task needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Ticket {
    priority: RequestPriority,
    seq: u64,
}

struct Shared {
    dedupe_key: String,
    cancel: CancelToken,
    state: Mutex<SharedState>,
    done: Condvar,
}

struct SharedState {
    waiters: usize,
    outcome: Option<Result<RuntimeResponse, RequestFailure>>,
}

struct EntryInner {
    phase: Phase,
    host: Option<Arc<dyn SemanticRuntimeHost>>,
    leases: usize,
    active: usize,
    last_activity: Instant,
    consecutive_failures: u32,
    retry_at: Option<Instant>,
    counters: Counters,
    running: bool,
    /// A host has been taken out of this entry and not yet shut down.
    /// Part of "at rest": a runtime whose dead host is still on its way
    /// out has not finished crashing.
    retiring: bool,
    queue: BTreeSet<Ticket>,
    inflight: HashMap<String, Arc<Shared>>,
}

struct RuntimeEntry {
    context: AnalysisContext,
    key: String,
    policy: RuntimePolicy,
    inner: Mutex<EntryInner>,
    /// Startup completion. Waiters on a cold context block here so only
    /// one launch happens.
    started: Condvar,
    /// Serial-host queue admission.
    gate: Condvar,
    /// Signalled when a request finishes and the entry's books are
    /// closed. What makes "this runtime has come back to rest" an event
    /// a caller can wait for instead of a state it has to poll for.
    quiet: Condvar,
    next_seq: AtomicU64,
}

impl RuntimeEntry {
    fn new(context: AnalysisContext, key: String, policy: RuntimePolicy) -> Self {
        Self {
            context,
            key,
            policy,
            inner: Mutex::new(EntryInner {
                phase: Phase::Cold,
                host: None,
                leases: 0,
                active: 0,
                last_activity: Instant::now(),
                consecutive_failures: 0,
                retry_at: None,
                counters: Counters::default(),
                running: false,
                retiring: false,
                queue: BTreeSet::new(),
                inflight: HashMap::new(),
            }),
            started: Condvar::new(),
            gate: Condvar::new(),
            quiet: Condvar::new(),
            next_seq: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, EntryInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn next_request_id(&self) -> RequestId {
        RequestId(self.next_seq.fetch_add(1, Ordering::SeqCst))
    }

    fn state_of(&self, inner: &EntryInner) -> RuntimeState {
        match inner.phase {
            Phase::Cold => RuntimeState::Cold,
            Phase::Starting => RuntimeState::Starting,
            Phase::Backoff => RuntimeState::Backoff,
            Phase::Degraded => RuntimeState::Degraded,
            Phase::Stopped => RuntimeState::Stopped,
            Phase::Ready => {
                if inner.active > 0 {
                    RuntimeState::Busy
                } else if inner.leases == 0
                    && inner.last_activity.elapsed() >= self.policy.idle_timeout
                {
                    RuntimeState::Idle
                } else {
                    RuntimeState::Ready
                }
            }
        }
    }

    /// Retire the host and enter backoff (or degrade). The caller holds
    /// the lock; the host's own `shutdown` runs after it is released,
    /// because a host may block.
    fn record_failure(&self, inner: &mut EntryInner) -> Option<Arc<dyn SemanticRuntimeHost>> {
        inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
        if inner.consecutive_failures >= self.policy.restart_budget {
            inner.phase = Phase::Degraded;
            inner.retry_at = None;
        } else {
            inner.phase = Phase::Backoff;
            inner.retry_at =
                Some(Instant::now() + self.policy.backoff_for(inner.consecutive_failures));
        }
        inner.host.take()
    }
}

// ---------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------

struct Registry {
    shutting_down: bool,
    entries: BTreeMap<String, Arc<RuntimeEntry>>,
}

/// Owns every semantic runtime, one per [`AnalysisContext`].
///
/// The registry lock is held only to find or insert an entry -- never
/// while a backend starts, and never while a request runs. A slow
/// request in one context cannot block acquiring, querying, or stopping
/// another.
pub struct SemanticRuntimeSupervisor {
    policy: RuntimePolicy,
    launchers: BTreeMap<SemanticBackendKind, Arc<dyn SemanticBackendLauncher>>,
    registry: Mutex<Registry>,
    /// Fleet-wide, because eviction is a decision about the fleet and
    /// not about the runtime that happened to be chosen.
    capacity_evictions: AtomicU64,
}

impl SemanticRuntimeSupervisor {
    #[must_use]
    pub fn new(policy: RuntimePolicy) -> Self {
        Self {
            policy,
            launchers: BTreeMap::new(),
            registry: Mutex::new(Registry {
                shutting_down: false,
                entries: BTreeMap::new(),
            }),
            capacity_evictions: AtomicU64::new(0),
        }
    }

    /// Register the launcher for one backend family. Registering does
    /// not start anything: a supervisor with five launchers and no
    /// query runs zero processes.
    #[must_use]
    pub fn with_backend(mut self, launcher: Arc<dyn SemanticBackendLauncher>) -> Self {
        self.launchers.insert(launcher.kind(), launcher);
        self
    }

    #[must_use]
    pub fn policy(&self) -> RuntimePolicy {
        self.policy
    }

    fn registry(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// How many logical runtimes exist, warm or stopped.
    #[must_use]
    pub fn runtime_count(&self) -> usize {
        self.registry().entries.len()
    }

    /// How many runtimes are currently holding a live host.
    #[must_use]
    pub fn live_runtime_count(&self) -> usize {
        self.entries()
            .iter()
            .filter(|entry| entry.lock().host.is_some())
            .count()
    }

    /// One runtime's state. An unknown context is [`RuntimeState::Cold`]
    /// -- never started is a state, not an error.
    #[must_use]
    pub fn state(&self, context_key: &str) -> RuntimeState {
        let Some(entry) = self.entry(context_key) else {
            return RuntimeState::Cold;
        };
        let inner = entry.lock();
        entry.state_of(&inner)
    }

    /// One runtime's telemetry, or `None` if it has never been acquired.
    #[must_use]
    pub fn telemetry(&self, context_key: &str) -> Option<RuntimeTelemetry> {
        let entry = self.entry(context_key)?;
        let inner = entry.lock();
        Some(RuntimeTelemetry {
            state: entry.state_of(&inner),
            starts_attempted: inner.counters.starts_attempted,
            starts_succeeded: inner.counters.starts_succeeded,
            restart_attempts: inner.counters.restart_attempts,
            crashes: inner.counters.crashes,
            consecutive_failures: inner.consecutive_failures,
            requests_started: inner.counters.requests_started,
            requests_completed: inner.counters.requests_completed,
            requests_cancelled: inner.counters.requests_cancelled,
            requests_timed_out: inner.counters.requests_timed_out,
            requests_failed: inner.counters.requests_failed,
            dedupe_hits: inner.counters.dedupe_hits,
            active_requests: inner.active,
            active_leases: inner.leases,
            queue_depth: inner.queue.len(),
            idle_for: inner.last_activity.elapsed(),
            latency_samples: inner.counters.latency.iter().copied().collect(),
            resource_usage: inner
                .host
                .as_ref()
                .map_or(ResourceUsage::UNAVAILABLE, |host| host.resource_usage()),
        })
    }

    fn entry(&self, context_key: &str) -> Option<Arc<RuntimeEntry>> {
        self.registry().entries.get(context_key).cloned()
    }

    /// Every known runtime, added up.
    ///
    /// Reading this starts nothing: it walks the entries that already
    /// exist, and a context nobody has ever acquired has no entry.
    #[must_use]
    pub fn fleet_telemetry(&self) -> FleetTelemetry {
        let entries = self.entries();
        let mut fleet = FleetTelemetry {
            known_contexts: entries.len(),
            capacity_evictions: self.capacity_evictions.load(Ordering::SeqCst),
            ..FleetTelemetry::default()
        };
        for entry in entries {
            let inner = entry.lock();
            match entry.state_of(&inner) {
                RuntimeState::Cold => fleet.cold += 1,
                RuntimeState::Starting => fleet.starting += 1,
                RuntimeState::Ready => fleet.ready += 1,
                RuntimeState::Busy => fleet.busy += 1,
                RuntimeState::Idle => fleet.idle += 1,
                RuntimeState::Backoff => fleet.backoff += 1,
                RuntimeState::Degraded => fleet.degraded += 1,
                RuntimeState::Stopped => fleet.stopped += 1,
            }
            fleet.active_requests += inner.active;
            fleet.active_leases += inner.leases;
            fleet.queue_depth += inner.queue.len();
            fleet.starts_attempted += inner.counters.starts_attempted;
            fleet.starts_succeeded += inner.counters.starts_succeeded;
            fleet.restart_attempts += inner.counters.restart_attempts;
            fleet.crashes += inner.counters.crashes;
            fleet.requests_started += inner.counters.requests_started;
            fleet.requests_completed += inner.counters.requests_completed;
            fleet.requests_cancelled += inner.counters.requests_cancelled;
            fleet.requests_timed_out += inner.counters.requests_timed_out;
            fleet.requests_failed += inner.counters.requests_failed;
            fleet.dedupe_hits += inner.counters.dedupe_hits;

            let Some(host) = inner.host.as_ref() else {
                continue;
            };
            fleet.live_runtimes += 1;
            let usage = host.resource_usage();
            match usage.rss_bytes {
                Some(bytes) => {
                    fleet.known_rss_bytes =
                        Some(fleet.known_rss_bytes.unwrap_or(0).saturating_add(bytes));
                }
                // An unmeasurable host is counted as unmeasured, and
                // never added in as a zero.
                None => fleet.resource_usage_unknown += 1,
            }
            if let Some(millis) = usage.cpu_millis {
                fleet.known_cpu_millis =
                    Some(fleet.known_cpu_millis.unwrap_or(0).saturating_add(millis));
            }
        }
        fleet
    }

    /// Block until no request is running in this context, or until
    /// `timeout`. Returns whether it came to rest.
    ///
    /// An event, not a poll: a request signals when its books are
    /// closed, so a caller never has to guess how long "a moment later"
    /// is. `timeout` is a safety net for a wedged backend, never the
    /// synchronization itself.
    ///
    /// An unknown context is at rest, because it is not running
    /// anything.
    pub fn await_quiescent(&self, context_key: &str, timeout: Duration) -> bool {
        let Some(entry) = self.entry(context_key) else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        let mut inner = entry.lock();
        while inner.active > 0 || inner.retiring {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (next, _) = entry
                .quiet
                .wait_timeout(inner, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inner = next;
        }
        true
    }

    fn entries(&self) -> Vec<Arc<RuntimeEntry>> {
        self.registry().entries.values().cloned().collect()
    }

    /// Retire idle runtimes until one more host fits under `limit`.
    ///
    /// `mine` is the entry already reserved in STARTING for the caller,
    /// so it counts against the limit and is never itself a candidate.
    /// A runtime that is leased or running a request is never a
    /// candidate either, however old: capacity is a reason to retire
    /// something nobody is using, never a reason to take an answer away
    /// from a client who is waiting for one.
    ///
    /// Returns the hosts to shut down. The caller does that after every
    /// lock is released, because a host may block for a long time on
    /// its way out.
    fn make_room(
        &self,
        mine: &str,
        limit: usize,
    ) -> Result<Vec<Arc<dyn SemanticRuntimeHost>>, StartFailure> {
        /// Oldest first, then context key: a total order with no clock
        /// ties and no randomness, so the same fleet always evicts the
        /// same runtime.
        struct Candidate {
            last_activity: Instant,
            key: String,
            entry: Arc<RuntimeEntry>,
        }

        let mut occupancy = 1; // the caller's own reserved slot
        let mut candidates: Vec<Candidate> = Vec::new();
        for entry in self.entries() {
            if entry.key == mine {
                continue;
            }
            let inner = entry.lock();
            // STARTING occupies a slot it has not filled yet. Counting
            // only live hosts would let two cold acquisitions each see
            // room for one and produce two.
            if inner.host.is_none() && inner.phase != Phase::Starting {
                continue;
            }
            occupancy += 1;
            if inner.host.is_some()
                && inner.phase == Phase::Ready
                && inner.leases == 0
                && inner.active == 0
            {
                candidates.push(Candidate {
                    last_activity: inner.last_activity,
                    key: entry.key.clone(),
                    entry: Arc::clone(&entry),
                });
            }
        }
        if occupancy <= limit {
            return Ok(Vec::new());
        }

        candidates.sort_by(|left, right| {
            left.last_activity
                .cmp(&right.last_activity)
                .then_with(|| left.key.cmp(&right.key))
        });

        let mut retired = Vec::new();
        for candidate in candidates {
            if occupancy <= limit {
                break;
            }
            let host = {
                let mut inner = candidate.entry.lock();
                // Re-checked under the lock: a client may have taken a
                // lease since the snapshot, and that client wins.
                if inner.phase != Phase::Ready || inner.leases > 0 || inner.active > 0 {
                    continue;
                }
                let Some(host) = inner.host.take() else {
                    continue;
                };
                inner.phase = Phase::Stopped;
                host
            };
            self.capacity_evictions.fetch_add(1, Ordering::SeqCst);
            retired.push(host);
            occupancy -= 1;
        }

        if occupancy > limit {
            // Everything left is leased or working. Say so, in a word
            // that is not "the backend failed".
            for host in retired {
                host.shutdown();
            }
            return Err(StartFailure::Capacity {
                limit,
                live: occupancy,
            });
        }
        Ok(retired)
    }

    /// Get a lease on the runtime for `binding`, starting it if needed.
    ///
    /// Lazy: the first caller for a cold context starts it, and
    /// concurrent callers for the same cold context wait for that one
    /// startup rather than starting their own.
    pub fn acquire(&self, binding: &AnalysisContextBinding) -> Result<RuntimeLease, StartFailure> {
        let key = binding.context.context_key();
        let launcher = self
            .launchers
            .get(&binding.context.backend)
            .cloned()
            .ok_or(StartFailure::NoBackendRegistered(binding.context.backend))?;

        let entry = {
            let mut registry = self.registry();
            if registry.shutting_down {
                return Err(StartFailure::ShuttingDown);
            }
            Arc::clone(registry.entries.entry(key.clone()).or_insert_with(|| {
                Arc::new(RuntimeEntry::new(
                    binding.context.clone(),
                    key.clone(),
                    self.policy,
                ))
            }))
        };

        loop {
            let mut inner = entry.lock();
            match inner.phase {
                Phase::Ready => {
                    inner.leases += 1;
                    inner.last_activity = Instant::now();
                    return Ok(RuntimeLease {
                        entry: Arc::clone(&entry),
                    });
                }
                Phase::Starting => {
                    // Someone else is launching: wait for their result
                    // rather than launching a second backend.
                    inner = entry
                        .started
                        .wait(inner)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    drop(inner);
                }
                Phase::Backoff => {
                    let now = Instant::now();
                    match inner.retry_at {
                        Some(at) if now < at => {
                            return Err(StartFailure::InBackoff {
                                attempts: inner.consecutive_failures,
                                retry_after: at - now,
                            });
                        }
                        _ => {
                            inner.phase = Phase::Cold;
                            drop(inner);
                        }
                    }
                }
                Phase::Degraded => {
                    return Err(StartFailure::Degraded {
                        attempts: inner.consecutive_failures,
                    });
                }
                Phase::Cold | Phase::Stopped => {
                    // STARTING first: it reserves this context's slot,
                    // so concurrent callers wait on `started` instead of
                    // launching a second backend, and so the capacity
                    // arithmetic below counts a launch that has not
                    // finished.
                    let previous = inner.phase;
                    inner.phase = Phase::Starting;
                    drop(inner);

                    if let Some(limit) = self.policy.max_live_runtimes {
                        match self.make_room(&key, limit) {
                            Ok(retired) => {
                                for host in retired {
                                    host.shutdown();
                                }
                            }
                            Err(failure) => {
                                // The fleet is full of work nobody may
                                // take away. Give the slot back exactly
                                // as it was: no start was attempted, so
                                // no restart budget was spent.
                                let mut inner = entry.lock();
                                inner.phase = previous;
                                drop(inner);
                                entry.started.notify_all();
                                return Err(failure);
                            }
                        }
                    }

                    let mut inner = entry.lock();
                    inner.counters.starts_attempted += 1;
                    if inner.consecutive_failures > 0 {
                        inner.counters.restart_attempts += 1;
                    }
                    drop(inner);

                    let launched = launcher.launch(binding);

                    let mut inner = entry.lock();
                    match launched {
                        Ok(host) => {
                            inner.counters.starts_succeeded += 1;
                            inner.host = Some(host);
                            inner.phase = Phase::Ready;
                            inner.retry_at = None;
                            inner.last_activity = Instant::now();
                        }
                        Err(error) => {
                            entry.record_failure(&mut inner);
                            drop(inner);
                            entry.started.notify_all();
                            return Err(StartFailure::Backend(error));
                        }
                    }
                    drop(inner);
                    entry.started.notify_all();

                    // Shutdown may have run while this launch was in
                    // flight. A host started past that point belongs to
                    // nobody, so it is released here rather than left
                    // READY behind a supervisor that is gone.
                    if self.registry().shutting_down {
                        let orphan = {
                            let mut inner = entry.lock();
                            inner.phase = Phase::Stopped;
                            inner.host.take()
                        };
                        if let Some(host) = orphan {
                            host.shutdown();
                        }
                        return Err(StartFailure::ShuttingDown);
                    }
                }
            }
        }
    }

    /// Unload every runtime that is warm, unleased, and past the idle
    /// policy. Returns how many were unloaded.
    ///
    /// A runtime with a live lease or a running request is never
    /// unloaded, however long it has been since the last activity.
    pub fn sweep_idle(&self) -> usize {
        let entries = self.entries();
        let mut unloaded = 0;
        for entry in entries {
            let retired = {
                let mut inner = entry.lock();
                let idle = inner.phase == Phase::Ready
                    && inner.leases == 0
                    && inner.active == 0
                    && inner.last_activity.elapsed() >= self.policy.idle_timeout;
                if !idle {
                    continue;
                }
                inner.phase = Phase::Stopped;
                inner.host.take()
            };
            if let Some(host) = retired {
                host.shutdown();
                unloaded += 1;
            }
        }
        unloaded
    }

    /// Stop taking work, cancel what is in flight, and release every
    /// host.
    ///
    /// Nothing persistent is touched: no semantic result is published
    /// yet, and structural truth is not this module's to disturb.
    pub fn shutdown(&self) {
        let entries: Vec<Arc<RuntimeEntry>> = {
            let mut registry = self.registry();
            registry.shutting_down = true;
            registry.entries.values().cloned().collect()
        };
        // Every host leaves through a `take` under the entry lock, so a
        // concurrent capacity eviction and this loop cannot both get the
        // same one: exactly one of them shuts it down, exactly once.

        for entry in entries {
            let (host, waiting) = {
                let mut inner = entry.lock();
                inner.phase = Phase::Stopped;
                let waiting: Vec<Arc<Shared>> = inner.inflight.values().cloned().collect();
                (inner.host.take(), waiting)
            };
            for shared in waiting {
                shared.cancel.cancel();
            }
            if let Some(host) = host {
                host.shutdown();
            }
            entry.started.notify_all();
            entry.gate.notify_all();
        }
    }
}

impl Drop for SemanticRuntimeSupervisor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------
// Leases and request execution
// ---------------------------------------------------------------------

/// Proof that someone is using a runtime.
///
/// While a lease is alive the runtime is never idle-unloaded. Dropping
/// it does not stop anything -- it just lets the idle policy apply
/// again, which is what makes "Agent A left, Agent B arrived a second
/// later" reuse one warm runtime.
pub struct RuntimeLease {
    entry: Arc<RuntimeEntry>,
}

impl RuntimeLease {
    #[must_use]
    pub fn context_key(&self) -> &str {
        &self.entry.key
    }

    #[must_use]
    pub fn context(&self) -> &AnalysisContext {
        &self.entry.context
    }

    #[must_use]
    pub fn state(&self) -> RuntimeState {
        let inner = self.entry.lock();
        self.entry.state_of(&inner)
    }

    /// Run a request and wait for it, bounded by `options`.
    pub fn execute(
        &self,
        request: RuntimeRequest,
        options: RequestOptions,
    ) -> Result<RuntimeResponse, RequestFailure> {
        self.submit(request)?.wait(options)
    }

    /// Start a request without waiting for it, so the caller can cancel
    /// it, wait with its own deadline, or hand the guard elsewhere.
    pub fn submit(&self, request: RuntimeRequest) -> Result<PendingRequest, RequestFailure> {
        let entry = Arc::clone(&self.entry);
        let dedupe_key = request.key.dedupe_key();

        let (shared, work) = {
            let mut inner = entry.lock();
            if inner.phase != Phase::Ready {
                return Err(RequestFailure::NotReady(entry.state_of(&inner)));
            }
            let host = inner
                .host
                .clone()
                .ok_or(RequestFailure::NotReady(RuntimeState::Stopped))?;

            if let Some(existing) = inner.inflight.get(&dedupe_key).cloned() {
                // The identical question is already being asked. Join
                // it instead of asking the backend twice.
                inner.counters.dedupe_hits += 1;
                let mut state = existing
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.waiters += 1;
                drop(state);
                (existing, None)
            } else {
                let shared = Arc::new(Shared {
                    dedupe_key: dedupe_key.clone(),
                    cancel: CancelToken::new(),
                    state: Mutex::new(SharedState {
                        waiters: 1,
                        outcome: None,
                    }),
                    done: Condvar::new(),
                });
                inner.inflight.insert(dedupe_key, Arc::clone(&shared));
                inner.counters.requests_started += 1;
                inner.active += 1;
                inner.last_activity = Instant::now();
                (shared, Some(host))
            }
        };

        if let Some(host) = work {
            let _id = entry.next_request_id();
            let worker_entry = Arc::clone(&entry);
            let worker_shared = Arc::clone(&shared);
            thread::spawn(move || {
                run_request(&worker_entry, host.as_ref(), &request, &worker_shared);
            });
        }

        Ok(PendingRequest {
            entry,
            shared: Some(shared),
        })
    }
}

impl fmt::Debug for RuntimeLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A lease renders as what it is a lease on. The host behind it
        // is not printable, and printing one would be the start of
        // leaking backend internals into a log.
        formatter
            .debug_struct("RuntimeLease")
            .field("context_key", &self.entry.key)
            .field("state", &self.state())
            .finish()
    }
}

impl Drop for RuntimeLease {
    fn drop(&mut self) {
        let mut inner = self.entry.lock();
        inner.leases = inner.leases.saturating_sub(1);
        inner.last_activity = Instant::now();
    }
}

/// One submitted request, not yet waited on.
///
/// Dropping or cancelling it withdraws this caller. The underlying work
/// keeps running while any other waiter still needs it -- one Agent
/// giving up is not a reason to throw away an answer another one is
/// waiting for, and never a reason to stop the shared runtime.
pub struct PendingRequest {
    entry: Arc<RuntimeEntry>,
    shared: Option<Arc<Shared>>,
}

impl PendingRequest {
    /// Wait for the answer.
    pub fn wait(mut self, options: RequestOptions) -> Result<RuntimeResponse, RequestFailure> {
        let shared = self.shared.take().expect("a pending request has its work");
        let deadline = options.timeout.map(|timeout| Instant::now() + timeout);

        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(outcome) = state.outcome.clone() {
                state.waiters = state.waiters.saturating_sub(1);
                return outcome;
            }
            let Some(deadline) = deadline else {
                state = shared
                    .done
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                continue;
            };
            let now = Instant::now();
            if now >= deadline {
                state.waiters = state.waiters.saturating_sub(1);
                let last = state.waiters == 0;
                drop(state);
                if last {
                    shared.cancel.cancel();
                }
                let mut inner = self.entry.lock();
                inner.counters.requests_timed_out += 1;
                return Err(RequestFailure::TimedOut {
                    after: options.timeout.unwrap_or_default(),
                });
            }
            let (next, _) = shared
                .done
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
        }
    }

    /// Withdraw this caller.
    pub fn cancel(mut self) {
        if let Some(shared) = self.shared.take() {
            self.withdraw(&shared);
        }
    }

    fn withdraw(&self, shared: &Arc<Shared>) {
        let last = {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.outcome.is_some() {
                state.waiters = state.waiters.saturating_sub(1);
                return;
            }
            state.waiters = state.waiters.saturating_sub(1);
            state.waiters == 0
        };
        let mut inner = self.entry.lock();
        inner.counters.requests_cancelled += 1;
        drop(inner);
        if last {
            // Nobody is left to receive this answer, so the backend may
            // stop working on it.
            shared.cancel.cancel();
        }
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.take() {
            self.withdraw(&shared);
        }
    }
}

/// The body of one backend execution, on its own thread.
fn run_request(
    entry: &Arc<RuntimeEntry>,
    host: &dyn SemanticRuntimeHost,
    request: &RuntimeRequest,
    shared: &Arc<Shared>,
) {
    let serial = host.concurrency() == HostConcurrency::Serial;
    let ticket = serial.then(|| admit(entry, request.priority));

    let started = Instant::now();
    let outcome = if shared.cancel.is_cancelled() {
        Err(RequestFailure::Cancelled)
    } else {
        // A backend that panics takes down its own thread and nothing
        // else: brainprintd, the supervisor, and every other context
        // keep running.
        match panic::catch_unwind(AssertUnwindSafe(|| host.execute(request, &shared.cancel))) {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(error)) => Err(RequestFailure::Backend(error)),
            Err(_) => Err(RequestFailure::Crashed(HostError::new(
                "backend panicked while running a request",
            ))),
        }
    };
    let latency = started.elapsed();

    if let Some(ticket) = ticket {
        release(entry, ticket);
    }

    let crashed =
        matches!(outcome, Err(RequestFailure::Crashed(_))) || host.health() != HostHealth::Healthy;

    // The runtime's own books close *before* any waiter is told, so
    // that a caller holding the answer is never holding it earlier than
    // the state the answer implies. Asserting BUSY-or-READY right after
    // a `wait` returned used to be a race for exactly this reason: the
    // answer arrived first and `active` came down a moment later.
    //
    // The two locks are not nested here, and every path that does nest
    // them takes the entry first, so there is no cycle either way.
    let retired = {
        let mut inner = entry.lock();
        inner.inflight.remove(&shared.dedupe_key);
        inner.active = inner.active.saturating_sub(1);
        inner.last_activity = Instant::now();
        inner.counters.sample(latency);
        match &outcome {
            Ok(_) => {
                inner.counters.requests_completed += 1;
                // A request that worked proves the runtime is healthy
                // again: the deterministic rule that clears a restart
                // budget spent on earlier crashes.
                inner.consecutive_failures = 0;
            }
            Err(RequestFailure::Cancelled) => inner.counters.requests_cancelled += 1,
            Err(_) => inner.counters.requests_failed += 1,
        }
        if crashed && inner.phase == Phase::Ready {
            inner.counters.crashes += 1;
            let retired = entry.record_failure(&mut inner);
            inner.retiring = retired.is_some();
            retired
        } else {
            None
        }
    };

    if retired.is_none() {
        entry.quiet.notify_all();
    }

    {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.outcome = Some(outcome);
    }
    shared.done.notify_all();

    // Last, and outside every lock: a host on its way out may block,
    // and no waiter should be kept from its answer by that. The
    // runtime is only at rest once it is gone, which is why the
    // quiescence signal waits for this and not for the answer.
    if let Some(host) = retired {
        host.shutdown();
        entry.lock().retiring = false;
        entry.quiet.notify_all();
        entry.gate.notify_all();
    }
}

/// Take a turn on a serial host, interactive work first.
fn admit(entry: &Arc<RuntimeEntry>, priority: RequestPriority) -> Ticket {
    let ticket = Ticket {
        priority,
        seq: entry.next_seq.fetch_add(1, Ordering::SeqCst),
    };
    let mut inner = entry.lock();
    inner.queue.insert(ticket);
    while inner.running || inner.queue.first() != Some(&ticket) {
        inner = entry
            .gate
            .wait(inner)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    inner.queue.remove(&ticket);
    inner.running = true;
    ticket
}

fn release(entry: &Arc<RuntimeEntry>, _ticket: Ticket) {
    let mut inner = entry.lock();
    inner.running = false;
    drop(inner);
    entry.gate.notify_all();
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
    };

    use brainprint_core::{ResourceId, WorkspaceId};

    use super::*;
    use crate::{
        config::WorkspaceConfig,
        resource::ResourceLanguage,
        scan::BaselineScan,
        semantic::{ProjectRootIdentity, ToolchainIdentity},
    };

    // -----------------------------------------------------------------
    // A deterministic fake backend.
    // -----------------------------------------------------------------

    /// What the fake does when asked.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Behavior {
        /// Answer immediately.
        Echo,
        /// Block until the test releases it, then answer.
        Blocked,
        /// Return unhealthy afterwards, as a crashed child would.
        CrashSilently,
        /// Panic inside the backend, as a wedged adapter would.
        Panic,
        /// Block until released, then die. The only way to get several
        /// waiters onto one execution and *then* crash it: a backend
        /// that crashes instantly has finished before the second
        /// caller could join.
        BlockedThenCrash,
        /// Wait at a shared barrier, then answer. Every request has to
        /// be inside the backend at once for any of them to leave, so
        /// this either proves overlap or hangs -- there is no timing
        /// window where it passes by luck.
        Rendezvous,
    }

    struct FakeHost {
        behavior: Mutex<Behavior>,
        healthy: AtomicBool,
        concurrency: HostConcurrency,
        shutdowns: Arc<AtomicUsize>,
        /// This host's own shutdowns, as opposed to the launcher-wide
        /// tally. A fleet may legitimately shut down five hosts; no
        /// host may legitimately be shut down twice.
        own_shutdowns: AtomicUsize,
        /// How many times a request found its cancel token set.
        cancellations_seen: Arc<AtomicUsize>,
        /// What this host reports about its memory, if anything.
        rss_bytes: Option<u64>,
        rendezvous: Option<Arc<Barrier>>,
        executions: Arc<AtomicUsize>,
        /// Test-driven release for [`Behavior::Blocked`].
        gate: Mutex<Option<mpsc::Receiver<()>>>,
        /// Order in which requests actually reached the backend.
        order: Arc<Mutex<Vec<String>>>,
        /// Signals the test that a request has entered the backend.
        entered: Arc<Mutex<Vec<String>>>,
        entered_signal: Arc<Condvar>,
    }

    impl SemanticRuntimeHost for FakeHost {
        fn execute(
            &self,
            request: &RuntimeRequest,
            cancel: &CancelToken,
        ) -> Result<RuntimeResponse, HostError> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            let label = String::from_utf8_lossy(&request.payload).into_owned();
            // Read the behavior *before* announcing arrival. A test that
            // waits for `await_entered` and then changes the behavior for
            // the next request must not be able to change this one's:
            // signalling first left a window where a request meant to
            // block echoed instead, and the runtime went READY under a
            // test asserting BUSY.
            let behavior = *self
                .behavior
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            {
                let mut entered = self
                    .entered
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                entered.push(label.clone());
            }
            self.entered_signal.notify_all();

            match behavior {
                Behavior::Echo => {}
                Behavior::Blocked => {
                    let gate = self
                        .gate
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(receiver) = gate.as_ref() {
                        let _ = receiver.recv();
                    }
                }
                Behavior::CrashSilently => {
                    self.healthy.store(false, Ordering::SeqCst);
                    return Err(HostError::new("backend exited"));
                }
                Behavior::Panic => {
                    self.healthy.store(false, Ordering::SeqCst);
                    panic!("fake backend wedged");
                }
                Behavior::BlockedThenCrash => {
                    let gate = self
                        .gate
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(receiver) = gate.as_ref() {
                        let _ = receiver.recv();
                    }
                    self.healthy.store(false, Ordering::SeqCst);
                    return Err(HostError::new("backend exited"));
                }
                Behavior::Rendezvous => {
                    if let Some(barrier) = self.rendezvous.as_ref() {
                        barrier.wait();
                    }
                }
            }

            {
                let mut order = self
                    .order
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                order.push(label.clone());
            }

            if cancel.is_cancelled() {
                // The cooperative stop actually reached the backend.
                // Counted rather than merely returned, because the
                // caller who would have seen the return value is by
                // definition the caller who walked away.
                self.cancellations_seen.fetch_add(1, Ordering::SeqCst);
                return Err(HostError::new("cancelled"));
            }
            Ok(RuntimeResponse {
                payload: format!("answer:{label}").into_bytes(),
            })
        }

        fn health(&self) -> HostHealth {
            if self.healthy.load(Ordering::SeqCst) {
                HostHealth::Healthy
            } else {
                HostHealth::Crashed
            }
        }

        fn shutdown(&self) {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            self.own_shutdowns.fetch_add(1, Ordering::SeqCst);
        }

        fn concurrency(&self) -> HostConcurrency {
            self.concurrency
        }

        fn resource_usage(&self) -> ResourceUsage {
            ResourceUsage {
                rss_bytes: self.rss_bytes,
                cpu_millis: None,
            }
        }
    }

    /// Launches [`FakeHost`]s and records what it was asked to launch.
    struct FakeLauncher {
        kind: SemanticBackendKind,
        behavior: Mutex<Behavior>,
        concurrency: HostConcurrency,
        /// What every host this launcher starts reports as its RSS.
        rss_bytes: Mutex<Option<u64>>,
        rendezvous: Mutex<Option<Arc<Barrier>>>,
        cancellations_seen: Arc<AtomicUsize>,
        launches: Arc<AtomicUsize>,
        shutdowns: Arc<AtomicUsize>,
        executions: Arc<AtomicUsize>,
        order: Arc<Mutex<Vec<String>>>,
        entered: Arc<Mutex<Vec<String>>>,
        entered_signal: Arc<Condvar>,
        /// Fails the next `n` launches, so backoff can be driven.
        failing_launches: Mutex<usize>,
        /// Handed to blocked hosts.
        gate: Mutex<Option<mpsc::Receiver<()>>>,
        /// Blocks inside `launch`, to prove startup coalescing.
        launch_barrier: Mutex<Option<Arc<Barrier>>>,
        /// Launches that have entered `launch` but not yet returned, so
        /// a test can wait for "it is starting" instead of assuming it.
        entered_launch: Arc<Mutex<usize>>,
        entered_launch_signal: Arc<Condvar>,
        hosts: Mutex<Vec<Arc<FakeHost>>>,
    }

    impl FakeLauncher {
        fn new(kind: SemanticBackendKind) -> Arc<Self> {
            Self::with_concurrency(kind, HostConcurrency::Serial)
        }

        fn parallel(kind: SemanticBackendKind) -> Arc<Self> {
            Self::with_concurrency(kind, HostConcurrency::Parallel)
        }

        fn with_concurrency(kind: SemanticBackendKind, concurrency: HostConcurrency) -> Arc<Self> {
            Arc::new(Self {
                kind,
                behavior: Mutex::new(Behavior::Echo),
                concurrency,
                rss_bytes: Mutex::new(None),
                rendezvous: Mutex::new(None),
                cancellations_seen: Arc::new(AtomicUsize::new(0)),
                launches: Arc::new(AtomicUsize::new(0)),
                shutdowns: Arc::new(AtomicUsize::new(0)),
                executions: Arc::new(AtomicUsize::new(0)),
                order: Arc::new(Mutex::new(Vec::new())),
                entered: Arc::new(Mutex::new(Vec::new())),
                entered_signal: Arc::new(Condvar::new()),
                failing_launches: Mutex::new(0),
                gate: Mutex::new(None),
                launch_barrier: Mutex::new(None),
                entered_launch: Arc::new(Mutex::new(0)),
                entered_launch_signal: Arc::new(Condvar::new()),
                hosts: Mutex::new(Vec::new()),
            })
        }

        fn set_behavior(&self, behavior: Behavior) {
            *self
                .behavior
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = behavior;
            for host in self
                .hosts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
            {
                *host
                    .behavior
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = behavior;
            }
        }

        fn fail_next_launches(&self, count: usize) {
            *self
                .failing_launches
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = count;
        }

        fn launch_count(&self) -> usize {
            self.launches.load(Ordering::SeqCst)
        }

        fn execution_count(&self) -> usize {
            self.executions.load(Ordering::SeqCst)
        }

        fn shutdown_count(&self) -> usize {
            self.shutdowns.load(Ordering::SeqCst)
        }

        /// The most times any single host was shut down.
        fn worst_host_shutdown_count(&self) -> usize {
            self.hosts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .map(|host| host.own_shutdowns.load(Ordering::SeqCst))
                .max()
                .unwrap_or(0)
        }

        /// How many requests were told, inside the backend, to stop.
        fn cancellations_seen(&self) -> usize {
            self.cancellations_seen.load(Ordering::SeqCst)
        }

        /// Hand the next host this test's release channel.
        fn gate_with(&self, gate: mpsc::Receiver<()>) {
            *self
                .gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(gate);
        }

        fn report_rss(&self, bytes: Option<u64>) {
            *self
                .rss_bytes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = bytes;
        }

        fn rendezvous_at(&self, barrier: &Arc<Barrier>) {
            *self
                .rendezvous
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(barrier));
        }

        fn order(&self) -> Vec<String> {
            self.order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        /// Block until `count` launches have entered the launcher.
        fn await_launching(&self, count: usize) {
            let mut entered = self
                .entered_launch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while *entered < count {
                entered = self
                    .entered_launch_signal
                    .wait(entered)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }

        /// Block until `count` requests have entered the backend.
        fn await_entered(&self, count: usize) {
            let mut entered = self
                .entered
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while entered.len() < count {
                entered = self
                    .entered_signal
                    .wait(entered)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
    }

    /// A generous safety net for a wedged fake. Never the thing being
    /// waited on -- that is always an event.
    const PATIENCE: Duration = Duration::from_secs(30);

    /// Assert a runtime comes back to rest, and is READY when it does.
    ///
    /// A request's worker thread closes its books and *then* answers
    /// its waiter, so this runtime's own answer already implies READY.
    /// A *sibling* request may still be unwinding, though, which is
    /// what this waits for -- on the entry's quiescence signal, not on
    /// a sleep.
    fn settles_ready(supervisor: &SemanticRuntimeSupervisor, key: &str, lease: &RuntimeLease) {
        assert!(
            supervisor.await_quiescent(key, PATIENCE),
            "the runtime never came back to rest"
        );
        assert_eq!(lease.state(), RuntimeState::Ready);
    }

    impl SemanticBackendLauncher for FakeLauncher {
        fn kind(&self) -> SemanticBackendKind {
            self.kind
        }

        fn launch(
            &self,
            _binding: &AnalysisContextBinding,
        ) -> Result<Arc<dyn SemanticRuntimeHost>, HostError> {
            {
                let mut entered = self
                    .entered_launch
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *entered += 1;
            }
            self.entered_launch_signal.notify_all();

            let barrier = self
                .launch_barrier
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(barrier) = barrier {
                barrier.wait();
            }

            {
                let mut failing = self
                    .failing_launches
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if *failing > 0 {
                    *failing -= 1;
                    self.launches.fetch_add(1, Ordering::SeqCst);
                    return Err(HostError::new("backend refused to start"));
                }
            }

            self.launches.fetch_add(1, Ordering::SeqCst);
            let host = Arc::new(FakeHost {
                behavior: Mutex::new(
                    *self
                        .behavior
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                ),
                healthy: AtomicBool::new(true),
                concurrency: self.concurrency,
                shutdowns: Arc::clone(&self.shutdowns),
                own_shutdowns: AtomicUsize::new(0),
                cancellations_seen: Arc::clone(&self.cancellations_seen),
                rss_bytes: *self
                    .rss_bytes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                rendezvous: self
                    .rendezvous
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
                executions: Arc::clone(&self.executions),
                gate: Mutex::new(
                    self.gate
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take(),
                ),
                order: Arc::clone(&self.order),
                entered: Arc::clone(&self.entered),
                entered_signal: Arc::clone(&self.entered_signal),
            });
            self.hosts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(Arc::clone(&host));
            Ok(host)
        }
    }

    // -----------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------

    fn toolchain(environment: &str) -> ToolchainIdentity {
        ToolchainIdentity {
            backend_version: "1.0.0".to_owned(),
            backend_compatibility_class: "fake:1".to_owned(),
            environment_fingerprint: environment.to_owned(),
        }
    }

    fn binding_for(
        workspace: [u8; 16],
        root: ProjectRootIdentity,
        environment: &str,
    ) -> AnalysisContextBinding {
        AnalysisContextBinding {
            context: AnalysisContext {
                workspace: WorkspaceId::from_bytes(workspace),
                backend: SemanticBackendKind::Python,
                language: ResourceLanguage::Python,
                project_root: root,
                toolchain: toolchain(environment),
            },
            project_root_rel: "services/api".to_owned(),
            config_file_rel: Some("services/api/pyproject.toml".to_owned()),
        }
    }

    fn binding() -> AnalysisContextBinding {
        binding_for(
            [1; 16],
            ProjectRootIdentity::Config(ResourceId::from_bytes([2; 16])),
            "sha256:venv",
        )
    }

    /// The same shape, for another backend family. Used to prove that
    /// fleet-wide rules -- capacity above all -- treat every language
    /// the same.
    fn binding_of(
        kind: SemanticBackendKind,
        language: ResourceLanguage,
        root: &str,
    ) -> AnalysisContextBinding {
        AnalysisContextBinding {
            context: AnalysisContext {
                workspace: WorkspaceId::from_bytes([1; 16]),
                backend: kind,
                language,
                project_root: ProjectRootIdentity::Key(root.to_owned()),
                toolchain: toolchain("sha256:mixed"),
            },
            project_root_rel: root.to_owned(),
            config_file_rel: None,
        }
    }

    /// A supervisor over several fake families at once.
    fn fleet(policy: RuntimePolicy, launchers: &[Arc<FakeLauncher>]) -> SemanticRuntimeSupervisor {
        let mut supervisor = SemanticRuntimeSupervisor::new(policy);
        for launcher in launchers {
            supervisor =
                supervisor.with_backend(Arc::clone(launcher) as Arc<dyn SemanticBackendLauncher>);
        }
        supervisor
    }

    /// A request against a named context, so tests that span several
    /// contexts do not all key off the default one.
    fn request_in(
        context_key: &str,
        target: &str,
        basis: Option<&str>,
        priority: RequestPriority,
    ) -> RuntimeRequest {
        RuntimeRequest {
            key: SemanticRequestKey {
                context_key: context_key.to_owned(),
                capability: SemanticCapability::ImportBinding,
                target: target.to_owned(),
                basis_token: basis.map(ToOwned::to_owned),
            },
            priority,
            payload: target.as_bytes().to_vec(),
        }
    }

    fn supervisor(policy: RuntimePolicy) -> (SemanticRuntimeSupervisor, Arc<FakeLauncher>) {
        let launcher = FakeLauncher::new(SemanticBackendKind::Python);
        let supervisor = SemanticRuntimeSupervisor::new(policy)
            .with_backend(Arc::clone(&launcher) as Arc<dyn SemanticBackendLauncher>);
        (supervisor, launcher)
    }

    /// Warm runtimes stay until swept.
    fn patient() -> RuntimePolicy {
        RuntimePolicy {
            idle_timeout: Duration::from_secs(3600),
            ..RuntimePolicy::default()
        }
    }

    /// Any unused runtime is immediately idle-eligible, which makes idle
    /// tests deterministic without sleeping.
    fn impatient() -> RuntimePolicy {
        RuntimePolicy {
            idle_timeout: Duration::ZERO,
            ..RuntimePolicy::default()
        }
    }

    fn request(target: &str, priority: RequestPriority) -> RuntimeRequest {
        RuntimeRequest {
            key: SemanticRequestKey {
                context_key: binding().context.context_key(),
                capability: SemanticCapability::ImportBinding,
                target: target.to_owned(),
                basis_token: None,
            },
            priority,
            payload: target.as_bytes().to_vec(),
        }
    }

    fn ask(lease: &RuntimeLease, target: &str) -> Result<RuntimeResponse, RequestFailure> {
        lease.execute(
            request(target, RequestPriority::Interactive),
            RequestOptions::default(),
        )
    }

    // -----------------------------------------------------------------
    // Ownership, laziness and reuse (tests 1-8)
    // -----------------------------------------------------------------

    #[test]
    fn a_supervisor_starts_with_no_runtimes_at_all() {
        let (supervisor, launcher) = supervisor(patient());

        assert_eq!(supervisor.runtime_count(), 0);
        assert_eq!(launcher.launch_count(), 0, "registering starts nothing");
        assert_eq!(
            supervisor.state(&binding().context.context_key()),
            RuntimeState::Cold,
            "a context nobody asked about has no runtime"
        );
    }

    #[test]
    fn the_first_acquire_starts_one_runtime_and_the_next_reuses_it() {
        let (supervisor, launcher) = supervisor(patient());

        let lease = supervisor.acquire(&binding()).expect("start");
        assert_eq!(launcher.launch_count(), 1);
        assert_eq!(lease.state(), RuntimeState::Ready);

        let second = supervisor.acquire(&binding()).expect("reuse");
        assert_eq!(launcher.launch_count(), 1, "a warm runtime is reused");
        assert_eq!(second.context_key(), lease.context_key());
        assert_eq!(supervisor.runtime_count(), 1);
    }

    #[test]
    fn concurrent_acquires_of_one_cold_context_coalesce_into_one_startup() {
        let (supervisor, launcher) = supervisor(patient());
        // The first launcher call blocks until all three acquires are in
        // flight, so this is a real race and not a sequence.
        let barrier = Arc::new(Barrier::new(2));
        *launcher
            .launch_barrier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&barrier));

        let supervisor = Arc::new(supervisor);
        let started = Arc::new(Barrier::new(4));
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let supervisor = Arc::clone(&supervisor);
                let started = Arc::clone(&started);
                thread::spawn(move || {
                    started.wait();
                    supervisor.acquire(&binding()).map(|lease| {
                        let key = lease.context_key().to_owned();
                        drop(lease);
                        key
                    })
                })
            })
            .collect();

        started.wait();
        barrier.wait();

        let keys: Vec<String> = handles
            .into_iter()
            .map(|handle| handle.join().expect("thread").expect("acquire"))
            .collect();

        assert_eq!(launcher.launch_count(), 1, "exactly one startup");
        assert_eq!(supervisor.runtime_count(), 1);
        assert!(
            keys.windows(2).all(|pair| pair[0] == pair[1]),
            "every waiter observed the same runtime"
        );
    }

    #[test]
    fn nothing_about_the_caller_can_produce_a_second_runtime() {
        let (supervisor, launcher) = supervisor(patient());

        // Three "clients" differing in every way a client can differ --
        // which is no way at all, because AnalysisContext has no field
        // for one.
        let claude = supervisor.acquire(&binding()).expect("start");
        let codex = supervisor.acquire(&binding()).expect("reuse");
        let gemini = supervisor.acquire(&binding()).expect("reuse");

        assert_eq!(launcher.launch_count(), 1);
        assert_eq!(supervisor.runtime_count(), 1);
        assert_eq!(claude.context_key(), codex.context_key());
        assert_eq!(codex.context_key(), gemini.context_key());
    }

    #[test]
    fn a_different_worktree_project_or_toolchain_is_a_different_runtime() {
        let (supervisor, launcher) = supervisor(patient());

        let main = binding();
        let feature = binding_for(
            [9; 16],
            ProjectRootIdentity::Config(ResourceId::from_bytes([2; 16])),
            "sha256:venv",
        );
        let other_project = binding_for(
            [1; 16],
            ProjectRootIdentity::Config(ResourceId::from_bytes([7; 16])),
            "sha256:venv",
        );
        let other_toolchain = binding_for(
            [1; 16],
            ProjectRootIdentity::Config(ResourceId::from_bytes([2; 16])),
            "sha256:other-venv",
        );

        let leases: Vec<RuntimeLease> = [&main, &feature, &other_project, &other_toolchain]
            .into_iter()
            .map(|binding| supervisor.acquire(binding).expect("start"))
            .collect();

        assert_eq!(launcher.launch_count(), 4);
        assert_eq!(supervisor.runtime_count(), 4);

        let mut keys: Vec<&str> = leases.iter().map(RuntimeLease::context_key).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), 4, "same repo and paths is not the same context");
    }

    // -----------------------------------------------------------------
    // Warm / idle / lease (tests 9-12)
    // -----------------------------------------------------------------

    #[test]
    fn releasing_a_lease_leaves_the_runtime_warm() {
        let (supervisor, launcher) = supervisor(patient());

        let agent_a = supervisor.acquire(&binding()).expect("start");
        drop(agent_a);

        assert_eq!(
            supervisor.state(&binding().context.context_key()),
            RuntimeState::Ready,
            "an Agent leaving is not a shutdown"
        );

        let agent_b = supervisor.acquire(&binding()).expect("reuse");
        assert_eq!(launcher.launch_count(), 1);
        assert_eq!(agent_b.state(), RuntimeState::Ready);
    }

    #[test]
    fn an_idle_runtime_is_unloaded_and_lazily_restarted() {
        let (supervisor, launcher) = supervisor(impatient());
        let key = binding().context.context_key();

        drop(supervisor.acquire(&binding()).expect("start"));
        assert_eq!(supervisor.state(&key), RuntimeState::Idle);

        assert_eq!(supervisor.sweep_idle(), 1);
        assert_eq!(supervisor.state(&key), RuntimeState::Stopped);
        assert_eq!(launcher.shutdown_count(), 1, "the host was released");

        let revived = supervisor.acquire(&binding()).expect("restart");
        assert_eq!(revived.state(), RuntimeState::Ready);
        assert_eq!(
            launcher.launch_count(),
            2,
            "unloaded, then lazily restarted"
        );
    }

    #[test]
    fn an_active_lease_prevents_idle_unload() {
        let (supervisor, launcher) = supervisor(impatient());
        let held = supervisor.acquire(&binding()).expect("start");

        assert_eq!(supervisor.sweep_idle(), 0, "something is using it");
        assert_eq!(held.state(), RuntimeState::Ready);
        assert_eq!(launcher.shutdown_count(), 0);

        drop(held);
        assert_eq!(supervisor.sweep_idle(), 1);
    }

    // -----------------------------------------------------------------
    // Cancellation and timeout (tests 13-16)
    // -----------------------------------------------------------------

    #[test]
    fn cancelling_one_request_leaves_the_shared_runtime_and_other_work_alone() {
        let (supervisor, launcher) = supervisor(patient());
        let (release, gate) = mpsc::channel();
        *launcher
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(gate);
        launcher.set_behavior(Behavior::Blocked);

        let lease = supervisor.acquire(&binding()).expect("start");
        let cancelled = lease
            .submit(request("abandoned", RequestPriority::Interactive))
            .expect("submit");
        let kept = lease
            .submit(request("wanted", RequestPriority::Interactive))
            .expect("submit");

        launcher.await_entered(1);
        cancelled.cancel();

        // The runtime is still the same runtime, and still serving.
        assert_eq!(supervisor.runtime_count(), 1);
        assert_eq!(launcher.launch_count(), 1);

        let _ = release.send(());
        let _ = release.send(());
        let answer = kept.wait(RequestOptions::default()).expect("other work");
        assert_eq!(answer.payload, b"answer:wanted".to_vec());
        settles_ready(&supervisor, &binding().context.context_key(), &lease);
    }

    #[test]
    fn cancelling_one_waiter_of_shared_work_does_not_cancel_the_other() {
        let (supervisor, launcher) = supervisor(patient());
        let (release, gate) = mpsc::channel();
        *launcher
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(gate);
        launcher.set_behavior(Behavior::Blocked);

        let lease = supervisor.acquire(&binding()).expect("start");
        let first = lease
            .submit(request("shared", RequestPriority::Interactive))
            .expect("submit");
        let second = lease
            .submit(request("shared", RequestPriority::Interactive))
            .expect("join");

        launcher.await_entered(1);
        first.cancel();
        let _ = release.send(());

        let answer = second
            .wait(RequestOptions::default())
            .expect("the remaining waiter still gets its answer");
        assert_eq!(answer.payload, b"answer:shared".to_vec());
        assert_eq!(launcher.execution_count(), 1, "one execution, two callers");
    }

    #[test]
    fn a_timeout_is_typed_and_never_an_empty_answer() {
        let (supervisor, launcher) = supervisor(patient());
        let (release, gate) = mpsc::channel();
        *launcher
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(gate);
        launcher.set_behavior(Behavior::Blocked);

        let lease = supervisor.acquire(&binding()).expect("start");
        let outcome = lease.execute(
            request("slow", RequestPriority::Interactive),
            RequestOptions::with_timeout(Duration::from_millis(30)),
        );

        match outcome {
            Err(RequestFailure::TimedOut { .. }) => {}
            other => panic!("expected a typed timeout, got {other:?}"),
        }

        // The runtime was not killed, and the backend was not blamed.
        assert_eq!(lease.state(), RuntimeState::Busy);
        let _ = release.send(());

        launcher.set_behavior(Behavior::Echo);
        let answer = ask(&lease, "after-timeout").expect("still reusable");
        assert_eq!(answer.payload, b"answer:after-timeout".to_vec());
        assert_eq!(launcher.launch_count(), 1, "no restart was needed");

        let telemetry = supervisor
            .telemetry(&binding().context.context_key())
            .expect("telemetry");
        assert_eq!(telemetry.requests_timed_out, 1);
    }

    // -----------------------------------------------------------------
    // Crash, restart, backoff (tests 17-21)
    // -----------------------------------------------------------------

    #[test]
    fn a_crash_is_detected_without_taking_down_the_supervisor() {
        let (supervisor, launcher) = supervisor(patient());
        launcher.set_behavior(Behavior::CrashSilently);

        let lease = supervisor.acquire(&binding()).expect("start");
        let outcome = ask(&lease, "boom");
        assert!(outcome.is_err());

        assert_eq!(lease.state(), RuntimeState::Backoff);
        // The dead host leaves after its waiter is answered, so the
        // retirement is waited for rather than assumed to have already
        // happened.
        assert!(supervisor.await_quiescent(&binding().context.context_key(), PATIENCE));
        assert_eq!(launcher.shutdown_count(), 1, "the dead host was retired");

        let telemetry = supervisor
            .telemetry(&binding().context.context_key())
            .expect("telemetry");
        assert_eq!(telemetry.crashes, 1);
        assert_eq!(telemetry.consecutive_failures, 1);
    }

    #[test]
    fn a_panicking_backend_does_not_take_the_process_with_it() {
        let (supervisor, launcher) = supervisor(patient());
        launcher.set_behavior(Behavior::Panic);

        let lease = supervisor.acquire(&binding()).expect("start");
        match ask(&lease, "wedged") {
            Err(RequestFailure::Crashed(_)) => {}
            other => panic!("expected a typed crash, got {other:?}"),
        }

        // Still here, still answering questions about itself.
        assert_eq!(supervisor.runtime_count(), 1);
        assert_eq!(lease.state(), RuntimeState::Backoff);
    }

    #[test]
    fn a_crash_in_one_context_leaves_another_ready() {
        let (supervisor, launcher) = supervisor(patient());
        let healthy = binding_for(
            [5; 16],
            ProjectRootIdentity::Config(ResourceId::from_bytes([2; 16])),
            "sha256:venv",
        );

        let good = supervisor.acquire(&healthy).expect("start");
        launcher.set_behavior(Behavior::CrashSilently);
        let bad = supervisor.acquire(&binding()).expect("start");
        assert!(ask(&bad, "boom").is_err());

        assert_eq!(bad.state(), RuntimeState::Backoff);
        launcher.set_behavior(Behavior::Echo);
        assert!(
            ask(&good, "fine").is_ok(),
            "an unrelated context keeps working"
        );
        assert_eq!(good.state(), RuntimeState::Ready);
    }

    #[test]
    fn restart_is_bounded_deterministically_and_recovers() {
        let policy = RuntimePolicy {
            idle_timeout: Duration::from_secs(3600),
            restart_budget: 3,
            backoff_base: Duration::from_secs(60),
            backoff_max: Duration::from_secs(600),
            max_live_runtimes: None,
        };
        let (supervisor, launcher) = supervisor(policy);
        let key = binding().context.context_key();

        // Deterministic doubling, capped.
        assert_eq!(policy.backoff_for(0), Duration::ZERO);
        assert_eq!(policy.backoff_for(1), Duration::from_secs(60));
        assert_eq!(policy.backoff_for(2), Duration::from_secs(120));
        assert_eq!(policy.backoff_for(9), Duration::from_secs(600));

        launcher.fail_next_launches(1);
        match supervisor.acquire(&binding()) {
            Err(StartFailure::Backend(_)) => {}
            other => panic!("expected a start failure, got {other:?}"),
        }
        assert_eq!(supervisor.state(&key), RuntimeState::Backoff);

        // Inside the window, a retry is refused instead of storming.
        match supervisor.acquire(&binding()) {
            Err(StartFailure::InBackoff { attempts, .. }) => assert_eq!(attempts, 1),
            other => panic!("expected backoff, got {other:?}"),
        }
        assert_eq!(
            launcher.launch_count(),
            1,
            "a refused retry never reaches the launcher"
        );

        let telemetry = supervisor.telemetry(&key).expect("telemetry");
        assert_eq!(telemetry.starts_attempted, 1);
        assert_eq!(telemetry.starts_succeeded, 0);
        assert_eq!(telemetry.consecutive_failures, 1);
    }

    #[test]
    fn the_restart_budget_runs_out_into_a_degraded_runtime() {
        let policy = RuntimePolicy {
            idle_timeout: Duration::from_secs(3600),
            restart_budget: 2,
            backoff_base: Duration::ZERO,
            backoff_max: Duration::ZERO,
            max_live_runtimes: None,
        };
        let (supervisor, launcher) = supervisor(policy);
        let key = binding().context.context_key();

        launcher.fail_next_launches(5);
        for _ in 0..2 {
            assert!(supervisor.acquire(&binding()).is_err());
        }

        assert_eq!(supervisor.state(&key), RuntimeState::Degraded);
        match supervisor.acquire(&binding()) {
            Err(StartFailure::Degraded { attempts }) => assert_eq!(attempts, 2),
            other => panic!("expected degraded, got {other:?}"),
        }
        assert_eq!(
            launcher.launch_count(),
            2,
            "a degraded runtime stops trying, deterministically"
        );

        let telemetry = supervisor.telemetry(&key).expect("telemetry");
        assert_eq!(telemetry.starts_attempted, 2);
        assert_eq!(telemetry.restart_attempts, 1);
    }

    #[test]
    fn a_successful_restart_returns_the_runtime_to_service() {
        let policy = RuntimePolicy {
            idle_timeout: Duration::from_secs(3600),
            restart_budget: 4,
            backoff_base: Duration::ZERO,
            backoff_max: Duration::ZERO,
            max_live_runtimes: None,
        };
        let (supervisor, launcher) = supervisor(policy);

        launcher.set_behavior(Behavior::CrashSilently);
        let broken = supervisor.acquire(&binding()).expect("start");
        assert!(ask(&broken, "boom").is_err());
        drop(broken);

        launcher.set_behavior(Behavior::Echo);
        let restarted = supervisor.acquire(&binding()).expect("restart");
        assert_eq!(restarted.state(), RuntimeState::Ready);
        assert!(ask(&restarted, "recovered").is_ok());

        let telemetry = supervisor
            .telemetry(&binding().context.context_key())
            .expect("telemetry");
        assert_eq!(telemetry.starts_succeeded, 2);
        assert_eq!(telemetry.restart_attempts, 1);
        assert_eq!(
            telemetry.consecutive_failures, 0,
            "a request that worked clears the failure budget"
        );
    }

    // -----------------------------------------------------------------
    // Dedupe, priority, concurrency (tests 22-26)
    // -----------------------------------------------------------------

    #[test]
    fn identical_requests_share_one_execution_and_different_ones_do_not() {
        let (supervisor, launcher) = supervisor(patient());
        let (release, gate) = mpsc::channel();
        *launcher
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(gate);
        launcher.set_behavior(Behavior::Blocked);

        let lease = supervisor.acquire(&binding()).expect("start");
        let first = lease
            .submit(request("same", RequestPriority::Interactive))
            .expect("submit");
        let second = lease
            .submit(request("same", RequestPriority::Interactive))
            .expect("join");

        for _ in 0..2 {
            let _ = release.send(());
        }
        let one = first.wait(RequestOptions::default()).expect("answer");
        let two = second.wait(RequestOptions::default()).expect("answer");

        assert_eq!(one, two, "both waiters get the same normalized result");
        assert_eq!(launcher.execution_count(), 1);

        let telemetry = supervisor
            .telemetry(&binding().context.context_key())
            .expect("telemetry");
        assert_eq!(telemetry.dedupe_hits, 1);
        assert_eq!(telemetry.requests_started, 1);
    }

    #[test]
    fn different_request_keys_are_never_coalesced() {
        let base = SemanticRequestKey {
            context_key: "context".to_owned(),
            capability: SemanticCapability::ImportBinding,
            target: "pkg.mod.name".to_owned(),
            basis_token: None,
        };

        let mut other_capability = base.clone();
        other_capability.capability = SemanticCapability::References;
        let mut other_target = base.clone();
        other_target.target = "pkg.mod.other".to_owned();
        let mut other_context = base.clone();
        other_context.context_key = "elsewhere".to_owned();
        let mut with_basis = base.clone();
        with_basis.basis_token = Some(String::new());

        let mut keys: Vec<String> = [
            &base,
            &other_capability,
            &other_target,
            &other_context,
            &with_basis,
        ]
        .iter()
        .map(|key| key.dedupe_key())
        .collect();
        let total = keys.len();
        keys.sort();
        keys.dedup();

        assert_eq!(keys.len(), total, "no two distinct questions share a key");
        assert_eq!(
            base.dedupe_key(),
            base.clone().dedupe_key(),
            "the same question is the same key"
        );
    }

    #[test]
    fn interactive_work_is_not_trapped_behind_a_background_backlog() {
        let (supervisor, launcher) = supervisor(patient());
        let (release, gate) = mpsc::channel();
        *launcher
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(gate);
        launcher.set_behavior(Behavior::Blocked);

        let lease = supervisor.acquire(&binding()).expect("start");
        // One background request occupies the serial host...
        let running = lease
            .submit(request("bg-running", RequestPriority::Background))
            .expect("submit");
        launcher.await_entered(1);

        // ...and more background work queues behind it, before an
        // interactive question arrives.
        let queued: Vec<PendingRequest> = (0..3)
            .map(|index| {
                lease
                    .submit(request(
                        &format!("bg-queued-{index}"),
                        RequestPriority::Background,
                    ))
                    .expect("submit")
            })
            .collect();
        // Give the queued workers a turn to register their tickets.
        while supervisor
            .telemetry(&binding().context.context_key())
            .expect("telemetry")
            .queue_depth
            < 3
        {
            thread::yield_now();
        }

        let interactive = lease
            .submit(request("interactive", RequestPriority::Interactive))
            .expect("submit");
        while supervisor
            .telemetry(&binding().context.context_key())
            .expect("telemetry")
            .queue_depth
            < 4
        {
            thread::yield_now();
        }

        for _ in 0..5 {
            let _ = release.send(());
        }
        running.wait(RequestOptions::default()).expect("answer");
        interactive
            .wait(RequestOptions::default())
            .expect("interactive answer");
        for pending in queued {
            pending.wait(RequestOptions::default()).expect("answer");
        }

        let order = launcher.order();
        let interactive_at = order
            .iter()
            .position(|label| label == "interactive")
            .expect("interactive ran");
        let last_background = order
            .iter()
            .rposition(|label| label.starts_with("bg-queued"))
            .expect("background ran");
        assert!(
            interactive_at < last_background,
            "interactive work went ahead of queued background work: {order:?}"
        );
    }

    #[test]
    fn one_slow_context_does_not_serialize_the_others() {
        let (supervisor, launcher) = supervisor(patient());
        let (release, gate) = mpsc::channel();
        *launcher
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(gate);
        launcher.set_behavior(Behavior::Blocked);

        let slow = supervisor.acquire(&binding()).expect("start");
        let blocked = slow
            .submit(request("slow", RequestPriority::Interactive))
            .expect("submit");
        launcher.await_entered(1);

        // Context A is mid-request. The registry must still serve
        // everyone else: acquire, query, sweep.
        let other = binding_for(
            [4; 16],
            ProjectRootIdentity::Config(ResourceId::from_bytes([2; 16])),
            "sha256:venv",
        );
        launcher.set_behavior(Behavior::Echo);
        let fast = supervisor.acquire(&other).expect("start while A is busy");
        assert_eq!(
            supervisor.state(&binding().context.context_key()),
            RuntimeState::Busy
        );
        assert_eq!(
            supervisor.sweep_idle(),
            0,
            "a sweep is not blocked by another context's request"
        );
        drop(fast);

        let _ = release.send(());
        blocked.wait(RequestOptions::default()).expect("answer");
    }

    // -----------------------------------------------------------------
    // Telemetry, shutdown, boundaries (tests 27-32)
    // -----------------------------------------------------------------

    #[test]
    fn telemetry_counts_the_whole_lifecycle() {
        let (supervisor, launcher) = supervisor(patient());
        let key = binding().context.context_key();

        let lease = supervisor.acquire(&binding()).expect("start");
        ask(&lease, "one").expect("answer");
        ask(&lease, "two").expect("answer");

        let telemetry = supervisor.telemetry(&key).expect("telemetry");
        assert_eq!(telemetry.starts_attempted, 1);
        assert_eq!(telemetry.starts_succeeded, 1);
        assert_eq!(telemetry.requests_started, 2);
        assert_eq!(telemetry.requests_completed, 2);
        assert_eq!(telemetry.requests_failed, 0);
        assert_eq!(telemetry.active_requests, 0);
        assert_eq!(telemetry.active_leases, 1);
        assert_eq!(telemetry.queue_depth, 0);
        assert_eq!(telemetry.state, RuntimeState::Ready);
        assert_eq!(telemetry.latency_samples.len(), 2);

        launcher.set_behavior(Behavior::CrashSilently);
        assert!(ask(&lease, "boom").is_err());
        let after = supervisor.telemetry(&key).expect("telemetry");
        assert_eq!(after.crashes, 1);
        assert_eq!(after.requests_failed, 1);
        assert_eq!(after.state, RuntimeState::Backoff);
    }

    #[test]
    fn unavailable_resource_measurements_are_unavailable_and_not_zero() {
        let (supervisor, _launcher) = supervisor(patient());
        let lease = supervisor.acquire(&binding()).expect("start");
        ask(&lease, "one").expect("answer");

        let telemetry = supervisor
            .telemetry(&binding().context.context_key())
            .expect("telemetry");
        assert_eq!(telemetry.resource_usage, ResourceUsage::UNAVAILABLE);
        assert!(telemetry.resource_usage.rss_bytes.is_none());
        assert!(telemetry.resource_usage.cpu_millis.is_none());
    }

    #[test]
    fn shutdown_releases_every_runtime_and_stops_taking_work() {
        let (supervisor, launcher) = supervisor(patient());
        let other = binding_for(
            [6; 16],
            ProjectRootIdentity::Config(ResourceId::from_bytes([2; 16])),
            "sha256:venv",
        );
        drop(supervisor.acquire(&binding()).expect("start"));
        drop(supervisor.acquire(&other).expect("start"));
        assert_eq!(launcher.launch_count(), 2);

        supervisor.shutdown();

        assert_eq!(launcher.shutdown_count(), 2, "every host was released");
        assert_eq!(supervisor.live_runtime_count(), 0);
        assert_eq!(
            supervisor.state(&binding().context.context_key()),
            RuntimeState::Stopped
        );
        match supervisor.acquire(&binding()) {
            Err(StartFailure::ShuttingDown) => {}
            other => panic!("expected refusal after shutdown, got {other:?}"),
        }
    }

    #[test]
    fn an_unregistered_backend_family_is_a_typed_refusal_not_a_panic() {
        let supervisor = SemanticRuntimeSupervisor::new(patient());
        match supervisor.acquire(&binding()) {
            Err(StartFailure::NoBackendRegistered(SemanticBackendKind::Python)) => {}
            other => panic!("expected a typed refusal, got {other:?}"),
        }
        assert_eq!(supervisor.runtime_count(), 0);
    }

    /// The structural index is untouched by anything the supervisor
    /// does: no backend payload, no runtime id, no raw output reaches
    /// `index.db`, and I2/I3 work with no supervisor in existence.
    #[test]
    fn structural_truth_neither_needs_nor_notices_a_semantic_runtime() {
        let base = env::temp_dir().join(format!(
            "brainprint-runtime-structural-{}-{:?}",
            std::process::id(),
            thread::current().id()
        ));
        let root = base.join("workspace");
        fs::create_dir_all(root.join("src")).expect("workspace");
        fs::write(root.join("src/app.py"), "def go():\n    return 1\n").expect("fixture");
        let db_path = base.join("data").join("index.db");

        // I2/I3 runs with no supervisor constructed at all.
        let scan = BaselineScan::open(&db_path).expect("index.db");
        scan.run_initial_scan(&root, &WorkspaceConfig::default(), "workspace-rev-1")
            .expect("baseline scan");
        drop(scan);
        let before = fs::read(&db_path).expect("index.db bytes");

        let (supervisor, _launcher) = supervisor(patient());
        let lease = supervisor.acquire(&binding()).expect("start");
        let answer = ask(&lease, "pkg.mod.name").expect("answer");
        assert_eq!(answer.payload, b"answer:pkg.mod.name".to_vec());
        drop(lease);
        supervisor.shutdown();

        let after = fs::read(&db_path).expect("index.db bytes");
        assert_eq!(
            before, after,
            "a semantic runtime persists nothing: no raw backend output, no runtime identity"
        );

        // And the structural index is still readable and unchanged.
        let scan = BaselineScan::open(&db_path).expect("index.db");
        drop(scan);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_runtime_request_carries_no_identity_a_semantic_result_could_inherit() {
        // The only identities on the wire are the caller's own: the
        // context key and the normalized target. There is no public
        // request id to leak, and the response carries a payload and
        // nothing else.
        let request = request("pkg.mod.name", RequestPriority::Interactive);
        assert_eq!(request.key.context_key, binding().context.context_key());

        let (supervisor, _launcher) = supervisor(patient());
        let lease = supervisor.acquire(&binding()).expect("start");
        let response = lease
            .execute(request.clone(), RequestOptions::default())
            .expect("answer");

        let rendered = format!("{response:?}");
        assert!(
            !rendered.contains("RequestId"),
            "runtime control identity never rides along with a result: {rendered}"
        );
        let fields = format!(
            "{:?}",
            RuntimeResponse {
                payload: Vec::new()
            }
        );
        assert_eq!(fields, "RuntimeResponse { payload: [] }");
    }

    #[test]
    fn a_request_against_a_stopped_runtime_is_typed_and_not_an_empty_answer() {
        let (supervisor, _launcher) = supervisor(impatient());
        let lease = supervisor.acquire(&binding()).expect("start");

        // The sweep cannot take a leased runtime, so stop it outright.
        supervisor.shutdown();
        match ask(&lease, "after-shutdown") {
            Err(RequestFailure::NotReady(RuntimeState::Stopped)) => {}
            other => panic!("expected a typed refusal, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // #19 task 14 -- five clients, one runtime
    //
    // Everything below synchronizes on an event: a Barrier, a Condvar,
    // a channel, or a state the backend itself announces. No test in
    // this section sleeps to make a race go away, because a race that
    // a sleep hides is a race that ships.
    // -----------------------------------------------------------------

    /// How many independent clients the task asks about. Five, not
    /// "several", because the counts below are exact.
    const CLIENTS: usize = 5;

    #[test]
    fn five_clients_racing_a_cold_context_produce_exactly_one_backend() {
        for round in 0..8 {
            let (supervisor, launcher) = supervisor(patient());
            let supervisor = Arc::new(supervisor);
            // Every client is released at the same instant, and the
            // launcher itself blocks until they all are -- so if the
            // supervisor let two of them launch, the second launch
            // would be a second `launch_count`, not a timing accident.
            let start = Arc::new(Barrier::new(CLIENTS));

            let keys: Vec<String> = thread::scope(|scope| {
                let handles: Vec<_> = (0..CLIENTS)
                    .map(|_| {
                        let supervisor = Arc::clone(&supervisor);
                        let start = Arc::clone(&start);
                        scope.spawn(move || {
                            start.wait();
                            let lease = supervisor.acquire(&binding()).expect("start");
                            lease.context_key().to_owned()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| handle.join().expect("client"))
                    .collect()
            });

            assert_eq!(
                launcher.launch_count(),
                1,
                "round {round}: five cold acquisitions must launch one backend"
            );
            assert_eq!(supervisor.live_runtime_count(), 1);
            assert_eq!(supervisor.runtime_count(), 1);
            assert!(
                keys.iter().all(|key| key == &keys[0]),
                "every client got a lease on the same runtime"
            );

            let fleet = supervisor.fleet_telemetry();
            assert_eq!(fleet.starts_attempted, 1);
            assert_eq!(fleet.starts_succeeded, 1);
            assert_eq!(fleet.live_runtimes, 1);
        }
    }

    #[test]
    fn five_clients_come_and_go_without_ever_naming_themselves() {
        let (supervisor, launcher) = supervisor(patient());
        let key = binding().context.context_key();

        let mut leases: Vec<RuntimeLease> = (0..CLIENTS)
            .map(|_| supervisor.acquire(&binding()).expect("start"))
            .collect();
        assert_eq!(launcher.launch_count(), 1);
        assert_eq!(
            supervisor.telemetry(&key).expect("telemetry").active_leases,
            CLIENTS
        );

        // One client disappears. That is not a shutdown.
        drop(leases.remove(0));
        assert_eq!(supervisor.live_runtime_count(), 1);
        assert_eq!(launcher.shutdown_count(), 0);
        assert!(ask(&leases[0], "still here").is_ok());

        // A client that arrives later joins the same warm runtime.
        let latecomer = supervisor.acquire(&binding()).expect("reuse");
        assert_eq!(launcher.launch_count(), 1, "nothing was started again");
        assert_eq!(latecomer.context_key(), key);

        drop(leases);
        drop(latecomer);
        assert_eq!(
            supervisor.live_runtime_count(),
            1,
            "an empty room is not a reason to close it"
        );
        assert_eq!(
            supervisor.state(&key),
            RuntimeState::Ready,
            "still warm, still nobody's"
        );
    }

    #[test]
    fn one_backend_family_with_two_project_roots_is_two_runtimes() {
        let (supervisor, launcher) = supervisor(patient());
        let api = binding_of(SemanticBackendKind::Python, ResourceLanguage::Python, "api");
        let jobs = binding_of(
            SemanticBackendKind::Python,
            ResourceLanguage::Python,
            "jobs",
        );

        let one = supervisor.acquire(&api).expect("start");
        let other = supervisor.acquire(&jobs).expect("start");

        assert_ne!(
            one.context_key(),
            other.context_key(),
            "same language, same backend, different project: different semantic world"
        );
        assert_eq!(launcher.launch_count(), 2);
        assert_eq!(supervisor.live_runtime_count(), 2);
    }

    // -----------------------------------------------------------------
    // Deduplication and waiter-scoped withdrawal
    // -----------------------------------------------------------------

    #[test]
    fn five_identical_questions_are_asked_once_and_answered_five_times() {
        let (supervisor, launcher) = supervisor(patient());
        let (release, gate) = mpsc::channel();
        *launcher
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(gate);
        launcher.set_behavior(Behavior::Blocked);
        let key = binding().context.context_key();

        let lease = supervisor.acquire(&binding()).expect("start");
        let pending: Vec<PendingRequest> = (0..CLIENTS)
            .map(|_| {
                lease
                    .submit(request_in(
                        &key,
                        "same",
                        Some("rev-1"),
                        RequestPriority::Interactive,
                    ))
                    .expect("submit")
            })
            .collect();

        launcher.await_entered(1);
        assert_eq!(
            launcher.execution_count(),
            1,
            "five callers, one question, one execution"
        );

        let telemetry = supervisor.telemetry(&key).expect("telemetry");
        assert_eq!(
            telemetry.requests_started, 1,
            "requests_started counts backend executions"
        );
        assert_eq!(
            telemetry.dedupe_hits,
            (CLIENTS - 1) as u64,
            "and dedupe_hits counts the waiters that joined one"
        );
        assert_eq!(telemetry.active_requests, 1);

        let _ = release.send(());
        for waiter in pending {
            let answer = waiter.wait(RequestOptions::default()).expect("answer");
            assert_eq!(answer.payload, b"answer:same".to_vec());
        }
        assert_eq!(launcher.execution_count(), 1);
    }

    #[test]
    fn a_question_about_a_newer_basis_never_joins_an_older_answer() {
        // A parallel host, so that the second question can be *inside*
        // the backend while the first still is. On a serial host the
        // proof would be about the queue rather than about dedupe.
        let launcher = FakeLauncher::parallel(SemanticBackendKind::Python);
        let supervisor = fleet(patient(), &[Arc::clone(&launcher)]);
        let (release, gate) = mpsc::channel();
        launcher.gate_with(gate);
        launcher.set_behavior(Behavior::Blocked);
        let key = binding().context.context_key();

        let lease = supervisor.acquire(&binding()).expect("start");
        let old = lease
            .submit(request_in(
                &key,
                "same",
                Some("rev-1"),
                RequestPriority::Interactive,
            ))
            .expect("submit");
        let new = lease
            .submit(request_in(
                &key,
                "same",
                Some("rev-2"),
                RequestPriority::Interactive,
            ))
            .expect("submit");

        launcher.await_entered(2);
        assert_eq!(
            launcher.execution_count(),
            2,
            "one generation's answer may never stand in for another's"
        );
        assert_eq!(
            supervisor.telemetry(&key).expect("telemetry").dedupe_hits,
            0
        );

        drop(release);
        assert!(old.wait(RequestOptions::default()).is_ok());
        assert!(new.wait(RequestOptions::default()).is_ok());
    }

    #[test]
    fn one_client_leaving_never_takes_another_clients_answer_with_it() {
        let (supervisor, launcher) = supervisor(patient());
        let (release, gate) = mpsc::channel();
        *launcher
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(gate);
        launcher.set_behavior(Behavior::Blocked);
        let key = binding().context.context_key();

        let lease = supervisor.acquire(&binding()).expect("start");
        let mut waiters: Vec<PendingRequest> = (0..CLIENTS)
            .map(|_| {
                lease
                    .submit(request_in(
                        &key,
                        "shared",
                        Some("rev-1"),
                        RequestPriority::Interactive,
                    ))
                    .expect("submit")
            })
            .collect();
        launcher.await_entered(1);

        // A cancels outright.
        waiters.remove(0).cancel();
        // B gives up waiting. A deadline that has already passed, so
        // the timeout is an arithmetic fact and not a race.
        match waiters
            .remove(0)
            .wait(RequestOptions::with_timeout(Duration::ZERO))
        {
            Err(RequestFailure::TimedOut { .. }) => {}
            other => panic!("expected a typed timeout, got {other:?}"),
        }

        // The work itself is untouched: still one execution, still
        // running, still not cancelled.
        assert_eq!(launcher.execution_count(), 1);
        assert_eq!(
            supervisor
                .telemetry(&key)
                .expect("telemetry")
                .active_requests,
            1,
            "two clients left; the question did not"
        );

        let _ = release.send(());
        for waiter in waiters {
            let answer = waiter.wait(RequestOptions::default()).expect("answer");
            assert_eq!(
                answer.payload,
                b"answer:shared".to_vec(),
                "C, D and E get the answer A and B stopped waiting for"
            );
        }

        settles_ready(&supervisor, &key, &lease);
        let telemetry = supervisor.telemetry(&key).expect("telemetry");
        assert_eq!(telemetry.requests_cancelled, 1, "A withdrew");
        assert_eq!(telemetry.requests_timed_out, 1, "B gave up");
        assert_eq!(telemetry.requests_completed, 1, "the backend answered");
        assert_eq!(telemetry.crashes, 0, "neither of those is a crash");
        assert_eq!(launcher.shutdown_count(), 0, "nor a reason to stop");
    }

    #[test]
    fn shared_work_stops_only_when_the_last_waiter_is_gone() {
        /// Five waiters on one execution; `withdrawn` of them leave.
        /// Returns what the backend was told, and what the survivors
        /// got.
        fn round(withdrawn: usize) -> (usize, Option<Vec<u8>>) {
            let (supervisor, launcher) = supervisor(patient());
            let (release, gate) = mpsc::channel();
            launcher.gate_with(gate);
            launcher.set_behavior(Behavior::Blocked);
            let key = binding().context.context_key();

            let lease = supervisor.acquire(&binding()).expect("start");
            let mut waiters: Vec<PendingRequest> = (0..CLIENTS)
                .map(|_| {
                    lease
                        .submit(request_in(
                            &key,
                            "shared",
                            Some("rev-1"),
                            RequestPriority::Interactive,
                        ))
                        .expect("submit")
                })
                .collect();
            launcher.await_entered(1);

            for _ in 0..withdrawn {
                waiters.remove(0).cancel();
            }
            let _ = release.send(());
            let answer = waiters.pop().map(|last| {
                last.wait(RequestOptions::default())
                    .expect("answer")
                    .payload
            });
            drop(waiters);
            assert!(
                supervisor.await_quiescent(&key, PATIENCE),
                "the execution never finished"
            );
            (launcher.cancellations_seen(), answer)
        }

        // Four leave, one stays: the backend is never told to stop, and
        // the client who stayed gets the answer the other four paid for.
        let (told_to_stop, answer) = round(CLIENTS - 1);
        assert_eq!(told_to_stop, 0, "one client still wanted this answer");
        assert_eq!(answer, Some(b"answer:shared".to_vec()));

        // All five leave: now, and only now, the work may stop.
        let (told_to_stop, answer) = round(CLIENTS);
        assert_eq!(
            told_to_stop, 1,
            "the last withdrawal is what reaches the backend"
        );
        assert_eq!(answer, None);
    }

    // -----------------------------------------------------------------
    // Scheduling
    // -----------------------------------------------------------------

    #[test]
    fn a_parallel_host_really_overlaps_and_still_refuses_to_ask_twice() {
        let launcher = FakeLauncher::parallel(SemanticBackendKind::Python);
        let supervisor = fleet(patient(), &[Arc::clone(&launcher)]);
        let key = binding().context.context_key();

        // One barrier for three: none of them can leave the backend
        // until all three are inside it. Serialized execution would
        // never finish, so finishing *is* the proof of overlap.
        let barrier = Arc::new(Barrier::new(3));
        launcher.rendezvous_at(&barrier);
        launcher.set_behavior(Behavior::Rendezvous);

        let lease = supervisor.acquire(&binding()).expect("start");
        let first = lease
            .submit(request_in(&key, "one", None, RequestPriority::Interactive))
            .expect("submit");
        let second = lease
            .submit(request_in(&key, "two", None, RequestPriority::Interactive))
            .expect("submit");
        launcher.await_entered(2);

        // Two of three have arrived, so neither can have finished:
        // "one" is certainly still in flight, and a fourth caller
        // asking it joins rather than asks again.
        let joiner = lease
            .submit(request_in(&key, "one", None, RequestPriority::Interactive))
            .expect("submit");
        assert_eq!(
            supervisor.telemetry(&key).expect("telemetry").dedupe_hits,
            1,
            "dedupe is not a property of serial hosts"
        );
        assert_eq!(
            supervisor
                .telemetry(&key)
                .expect("telemetry")
                .active_requests,
            2,
            "a parallel host's active count is what is really running"
        );

        // The third distinct question trips the barrier.
        let third = lease
            .submit(request_in(
                &key,
                "three",
                None,
                RequestPriority::Interactive,
            ))
            .expect("submit");

        assert!(first.wait(RequestOptions::default()).is_ok());
        assert!(second.wait(RequestOptions::default()).is_ok());
        assert!(third.wait(RequestOptions::default()).is_ok());
        assert_eq!(
            joiner
                .wait(RequestOptions::default())
                .expect("answer")
                .payload,
            b"answer:one".to_vec()
        );
        assert_eq!(
            launcher.execution_count(),
            3,
            "four callers, three distinct questions"
        );
    }

    // -----------------------------------------------------------------
    // Capacity
    // -----------------------------------------------------------------

    /// A cap of `limit` live runtimes, and warm runtimes that stay warm
    /// until capacity itself retires them.
    fn capped(limit: usize) -> RuntimePolicy {
        RuntimePolicy {
            max_live_runtimes: Some(limit),
            ..patient()
        }
    }

    #[test]
    fn the_default_policy_caps_nothing_because_nobody_has_measured_yet() {
        assert_eq!(RuntimePolicy::default().max_live_runtimes, None);
    }

    #[test]
    fn capacity_retires_what_nobody_is_using_and_starts_what_is_wanted() {
        let (supervisor, launcher) = supervisor(capped(2));
        let first = binding_of(SemanticBackendKind::Python, ResourceLanguage::Python, "a");
        let second = binding_of(SemanticBackendKind::Python, ResourceLanguage::Python, "b");
        let third = binding_of(SemanticBackendKind::Python, ResourceLanguage::Python, "c");

        let idle_one = supervisor.acquire(&first).expect("start");
        let kept = supervisor.acquire(&second).expect("start");
        assert_eq!(supervisor.live_runtime_count(), 2);

        // A is released entirely; B is still leased.
        drop(idle_one);

        let newcomer = supervisor.acquire(&third).expect("start under the cap");
        assert_eq!(
            supervisor.live_runtime_count(),
            2,
            "a completed acquisition never leaves the fleet over its cap"
        );
        assert_eq!(
            supervisor.state(&first.context.context_key()),
            RuntimeState::Stopped,
            "the unused runtime made room"
        );
        assert_eq!(
            supervisor.state(&second.context.context_key()),
            RuntimeState::Ready,
            "the leased one was never a candidate"
        );
        assert_eq!(newcomer.state(), RuntimeState::Ready);
        assert_eq!(launcher.shutdown_count(), 1);
        assert_eq!(
            supervisor.fleet_telemetry().capacity_evictions,
            1,
            "and the fleet says why it is one short"
        );
        drop(kept);
    }

    #[test]
    fn a_fleet_where_every_runtime_is_working_refuses_rather_than_kills() {
        let (supervisor, launcher) = supervisor(capped(2));
        let first = binding_of(SemanticBackendKind::Python, ResourceLanguage::Python, "a");
        let second = binding_of(SemanticBackendKind::Python, ResourceLanguage::Python, "b");
        let third = binding_of(SemanticBackendKind::Python, ResourceLanguage::Python, "c");

        // A is protected by a lease.
        let leased = supervisor.acquire(&first).expect("start");

        // B is protected by a request it is running. The release
        // channel is installed after A's host was built, so it reaches
        // B's -- the one that has to block.
        let (release, gate) = mpsc::channel();
        launcher.gate_with(gate);
        let busy = supervisor.acquire(&second).expect("start");
        launcher.set_behavior(Behavior::Blocked);
        let running = busy
            .submit(request_in(
                &second.context.context_key(),
                "slow",
                None,
                RequestPriority::Background,
            ))
            .expect("submit");
        launcher.await_entered(1);
        drop(busy); // no lease left, but the request is still running

        match supervisor.acquire(&third) {
            Err(StartFailure::Capacity { limit, live }) => {
                assert_eq!(limit, 2);
                assert_eq!(live, 3, "the caller's own slot is counted honestly");
            }
            other => panic!("expected an explicit capacity refusal, got {other:?}"),
        }

        assert_eq!(launcher.launch_count(), 2, "nothing was started");
        assert_eq!(launcher.shutdown_count(), 0, "and nothing was killed");
        assert_eq!(leased.state(), RuntimeState::Ready);
        assert_eq!(supervisor.live_runtime_count(), 2);
        // A capacity refusal is not a failed start: no budget was spent.
        let telemetry = supervisor
            .telemetry(&third.context.context_key())
            .expect("the entry exists");
        assert_eq!(telemetry.starts_attempted, 0);
        assert_eq!(telemetry.consecutive_failures, 0);
        assert_eq!(telemetry.state, RuntimeState::Cold);

        let _ = release.send(());
        assert!(running.wait(RequestOptions::default()).is_ok());
    }

    #[test]
    fn eviction_order_is_oldest_first_and_never_a_coin_toss() {
        // Three idle runtimes, acquired in a known order, so the oldest
        // last activity is a fact rather than a guess.
        let (supervisor, launcher) = supervisor(capped(3));
        let roots = ["a", "b", "c"];
        let bindings: Vec<AnalysisContextBinding> = roots
            .iter()
            .map(|root| binding_of(SemanticBackendKind::Python, ResourceLanguage::Python, root))
            .collect();
        for binding in &bindings {
            drop(supervisor.acquire(binding).expect("start"));
        }
        // Touch the first one, so it is no longer the oldest.
        drop(supervisor.acquire(&bindings[0]).expect("reuse"));
        assert_eq!(launcher.launch_count(), 3);

        let fourth = binding_of(SemanticBackendKind::Python, ResourceLanguage::Python, "d");
        let _newcomer = supervisor.acquire(&fourth).expect("start");

        assert_eq!(
            supervisor.state(&bindings[1].context.context_key()),
            RuntimeState::Stopped,
            "the least recently used runtime is the one that goes"
        );
        assert_eq!(
            supervisor.state(&bindings[0].context.context_key()),
            RuntimeState::Ready
        );
        assert_eq!(
            supervisor.state(&bindings[2].context.context_key()),
            RuntimeState::Ready
        );
        assert_eq!(supervisor.live_runtime_count(), 3);
    }

    #[test]
    fn the_capacity_cap_is_fleet_wide_and_privileges_no_language() {
        let python = FakeLauncher::new(SemanticBackendKind::Python);
        let rust = FakeLauncher::new(SemanticBackendKind::Rust);
        let csharp = FakeLauncher::new(SemanticBackendKind::CSharp);
        let supervisor = fleet(
            capped(2),
            &[Arc::clone(&python), Arc::clone(&rust), Arc::clone(&csharp)],
        );

        let py = binding_of(SemanticBackendKind::Python, ResourceLanguage::Python, "py");
        let rs = binding_of(SemanticBackendKind::Rust, ResourceLanguage::Rust, "rs");
        let cs = binding_of(SemanticBackendKind::CSharp, ResourceLanguage::CSharp, "cs");

        drop(supervisor.acquire(&py).expect("start"));
        drop(supervisor.acquire(&rs).expect("start"));
        assert_eq!(supervisor.live_runtime_count(), 2);

        // Not "two per family": two, full stop.
        let _held = supervisor.acquire(&cs).expect("start");
        assert_eq!(
            supervisor.live_runtime_count(),
            2,
            "a third family does not get a third slot"
        );
        assert_eq!(
            supervisor.state(&py.context.context_key()),
            RuntimeState::Stopped,
            "the oldest went, and it being Python is not why"
        );
        assert_eq!(python.shutdown_count(), 1);
        assert_eq!(rust.shutdown_count(), 0);
        assert_eq!(csharp.shutdown_count(), 0);
    }

    #[test]
    fn two_worktrees_are_two_runtimes_against_one_cap() {
        let (supervisor, _launcher) = supervisor(capped(2));
        let one = binding_for(
            [10; 16],
            ProjectRootIdentity::Config(ResourceId::from_bytes([2; 16])),
            "sha256:venv",
        );
        let two = binding_for(
            [11; 16],
            ProjectRootIdentity::Config(ResourceId::from_bytes([2; 16])),
            "sha256:venv",
        );

        let _first = supervisor.acquire(&one).expect("start");
        let _second = supervisor.acquire(&two).expect("start");

        assert_ne!(one.context.context_key(), two.context.context_key());
        assert_eq!(
            supervisor.live_runtime_count(),
            2,
            "identical project, identical toolchain, two worktrees, two runtimes"
        );
        let third = binding_of(SemanticBackendKind::Python, ResourceLanguage::Python, "x");
        match supervisor.acquire(&third) {
            Err(StartFailure::Capacity { .. }) => {}
            other => panic!("both worktrees count against the cap, got {other:?}"),
        }
    }

    #[test]
    fn an_idle_sweep_shuts_a_host_down_once_and_only_once() {
        let (supervisor, launcher) = supervisor(impatient());
        drop(supervisor.acquire(&binding()).expect("start"));

        assert_eq!(supervisor.sweep_idle(), 1);
        assert_eq!(supervisor.sweep_idle(), 0, "there is nothing left to sweep");
        supervisor.shutdown();

        assert_eq!(launcher.shutdown_count(), 1);
        assert_eq!(launcher.worst_host_shutdown_count(), 1);
    }

    // -----------------------------------------------------------------
    // Failure, isolation and recovery
    // -----------------------------------------------------------------

    #[test]
    fn one_crash_with_five_waiters_is_one_crash_and_five_failures() {
        let (supervisor, launcher) = supervisor(patient());
        let (release, gate) = mpsc::channel();
        launcher.gate_with(gate);
        let key = binding().context.context_key();
        let lease = supervisor.acquire(&binding()).expect("start");
        // Blocked first, so all five join one execution, and only then
        // does that execution die.
        launcher.set_behavior(Behavior::BlockedThenCrash);

        let waiters: Vec<PendingRequest> = (0..CLIENTS)
            .map(|_| {
                lease
                    .submit(request_in(
                        &key,
                        "doomed",
                        Some("rev-1"),
                        RequestPriority::Interactive,
                    ))
                    .expect("submit")
            })
            .collect();
        launcher.await_entered(1);
        assert_eq!(
            supervisor.telemetry(&key).expect("telemetry").dedupe_hits,
            (CLIENTS - 1) as u64
        );
        let _ = release.send(());

        for waiter in waiters {
            match waiter.wait(RequestOptions::default()) {
                Err(RequestFailure::Backend(_) | RequestFailure::Crashed(_)) => {}
                Ok(response) => panic!("a crash became an answer: {response:?}"),
                other => panic!("unexpected {other:?}"),
            }
        }

        assert!(
            supervisor.await_quiescent(&key, PATIENCE),
            "the dead host never finished leaving"
        );
        let telemetry = supervisor.telemetry(&key).expect("telemetry");
        assert_eq!(telemetry.crashes, 1, "one backend died, not five");
        assert_eq!(
            telemetry.consecutive_failures, 1,
            "and five clients did not spend five restarts"
        );
        assert_eq!(telemetry.state, RuntimeState::Backoff);
        assert_eq!(launcher.shutdown_count(), 1);
        assert_eq!(launcher.worst_host_shutdown_count(), 1);
    }

    #[test]
    fn a_crash_in_one_context_leaves_another_ready_deterministically() {
        // The task 11 flake, reconstructed. The old assertion sampled
        // the healthy runtime's state the instant its own `wait`
        // returned, and a request's books used to close a moment after
        // that -- so READY and BUSY were both reachable and neither was
        // wrong. The runtime now closes its books before it answers,
        // which makes the state the answer implies the state that is
        // observable. No sleep, no retry, no settle window.
        for round in 0..16 {
            let (supervisor, launcher) = supervisor(patient());
            let healthy = binding_for(
                [5; 16],
                ProjectRootIdentity::Config(ResourceId::from_bytes([2; 16])),
                "sha256:venv",
            );

            let good = supervisor.acquire(&healthy).expect("start");
            launcher.set_behavior(Behavior::CrashSilently);
            let bad = supervisor.acquire(&binding()).expect("start");
            assert!(ask(&bad, "boom").is_err());
            assert_eq!(bad.state(), RuntimeState::Backoff, "round {round}");

            launcher.set_behavior(Behavior::Echo);
            assert!(ask(&good, "fine").is_ok(), "round {round}");
            assert_eq!(
                good.state(),
                RuntimeState::Ready,
                "round {round}: an unrelated context is READY the moment it answers"
            );
            assert_eq!(
                supervisor
                    .telemetry(&healthy.context.context_key())
                    .expect("telemetry")
                    .crashes,
                0,
                "round {round}: the crash belonged to the other context"
            );
        }
    }

    #[test]
    fn five_clients_racing_a_restart_start_one_replacement() {
        let policy = RuntimePolicy {
            restart_budget: 5,
            backoff_base: Duration::ZERO,
            backoff_max: Duration::ZERO,
            ..patient()
        };
        let (supervisor, launcher) = supervisor(policy);
        let supervisor = Arc::new(supervisor);
        let key = binding().context.context_key();

        launcher.set_behavior(Behavior::CrashSilently);
        let broken = supervisor.acquire(&binding()).expect("start");
        assert!(ask(&broken, "boom").is_err());
        drop(broken);
        assert_eq!(supervisor.state(&key), RuntimeState::Backoff);
        launcher.set_behavior(Behavior::Echo);

        // The backoff window is zero, so every client is eligible at
        // once -- which is exactly the storm this must not become.
        let start = Arc::new(Barrier::new(CLIENTS));
        thread::scope(|scope| {
            for _ in 0..CLIENTS {
                let supervisor = Arc::clone(&supervisor);
                let start = Arc::clone(&start);
                scope.spawn(move || {
                    start.wait();
                    supervisor.acquire(&binding()).expect("restart");
                });
            }
        });

        assert_eq!(
            launcher.launch_count(),
            2,
            "one original, one replacement -- not one per client"
        );
        let telemetry = supervisor.telemetry(&key).expect("telemetry");
        assert_eq!(telemetry.restart_attempts, 1);
        assert_eq!(telemetry.starts_succeeded, 2);
        assert_eq!(supervisor.live_runtime_count(), 1);
    }

    #[test]
    fn a_degraded_context_stops_trying_and_leaves_the_rest_of_the_fleet_alone() {
        let policy = RuntimePolicy {
            restart_budget: 2,
            backoff_base: Duration::ZERO,
            backoff_max: Duration::ZERO,
            ..patient()
        };
        let broken = FakeLauncher::new(SemanticBackendKind::Python);
        let working = FakeLauncher::new(SemanticBackendKind::Rust);
        let supervisor = fleet(policy, &[Arc::clone(&broken), Arc::clone(&working)]);

        let failing = binding_of(SemanticBackendKind::Python, ResourceLanguage::Python, "py");
        let healthy = binding_of(SemanticBackendKind::Rust, ResourceLanguage::Rust, "rs");

        broken.fail_next_launches(100);
        for _ in 0..2 {
            assert!(supervisor.acquire(&failing).is_err());
        }
        assert_eq!(
            supervisor.state(&failing.context.context_key()),
            RuntimeState::Degraded
        );

        // Five more clients ask. A degraded context answers all five
        // the same way, and reaches the launcher none of the times.
        for _ in 0..CLIENTS {
            match supervisor.acquire(&failing) {
                Err(StartFailure::Degraded { attempts }) => assert_eq!(attempts, 2),
                other => panic!("expected degraded, got {other:?}"),
            }
        }
        assert_eq!(
            broken.launch_count(),
            2,
            "a spent budget is not an invitation to loop"
        );

        let alive = supervisor
            .acquire(&healthy)
            .expect("another family is fine");
        assert!(ask(&alive, "unaffected").is_ok());
        assert_eq!(working.launch_count(), 1);

        let fleet = supervisor.fleet_telemetry();
        assert_eq!(fleet.degraded, 1);
        assert_eq!(fleet.ready, 1);
        assert_eq!(fleet.live_runtimes, 1, "partial availability, stated");
    }

    #[test]
    fn a_context_that_is_still_starting_does_not_hold_up_another() {
        let slow = FakeLauncher::new(SemanticBackendKind::CSharp);
        let quick = FakeLauncher::new(SemanticBackendKind::TypeScriptJavaScript);
        let supervisor = Arc::new(fleet(patient(), &[Arc::clone(&slow), Arc::clone(&quick)]));

        // The slow family's launch blocks until this test releases it.
        let held = Arc::new(Barrier::new(2));
        *slow
            .launch_barrier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&held));

        let slow_binding = binding_of(SemanticBackendKind::CSharp, ResourceLanguage::CSharp, "cs");
        let quick_binding = binding_of(
            SemanticBackendKind::TypeScriptJavaScript,
            ResourceLanguage::TypeScript,
            "ts",
        );

        thread::scope(|scope| {
            let starting = {
                let supervisor = Arc::clone(&supervisor);
                let slow_binding = slow_binding.clone();
                scope.spawn(move || supervisor.acquire(&slow_binding).expect("start"))
            };

            // Wait for C# to actually be inside `launch`, rather than
            // assuming the thread above got there first.
            slow.await_launching(1);

            // While C# is stuck inside `launch`, TypeScript starts and
            // answers. If the registry lock were held across a launch,
            // this would block until the barrier below released it.
            let ready = supervisor.acquire(&quick_binding).expect("start");
            assert_eq!(ready.state(), RuntimeState::Ready);
            assert!(ask(&ready, "not waiting").is_ok());
            assert_eq!(
                supervisor.state(&slow_binding.context.context_key()),
                RuntimeState::Starting,
                "the other one is still coming up"
            );

            held.wait();
            let arrived = starting.join().expect("slow start");
            assert_eq!(arrived.state(), RuntimeState::Ready);
        });

        assert_eq!(supervisor.live_runtime_count(), 2);
    }

    #[test]
    fn one_missing_backend_family_does_not_make_the_fleet_unavailable() {
        let present = FakeLauncher::new(SemanticBackendKind::Python);
        let supervisor = fleet(patient(), &[Arc::clone(&present)]);

        // Svelte has no launcher here at all -- the shape of a
        // toolchain that is simply not installed.
        let missing = binding_of(SemanticBackendKind::Svelte, ResourceLanguage::Svelte, "web");
        match supervisor.acquire(&missing) {
            Err(StartFailure::NoBackendRegistered(SemanticBackendKind::Svelte)) => {}
            other => panic!("expected a typed refusal, got {other:?}"),
        }

        let usable = supervisor
            .acquire(&binding_of(
                SemanticBackendKind::Python,
                ResourceLanguage::Python,
                "py",
            ))
            .expect("the installed family still starts");
        assert!(ask(&usable, "fine").is_ok());

        let fleet = supervisor.fleet_telemetry();
        assert_eq!(
            fleet.known_contexts, 1,
            "a family with no launcher never became a runtime entry"
        );
        assert_eq!(fleet.live_runtimes, 1);
    }

    // -----------------------------------------------------------------
    // Fleet telemetry
    // -----------------------------------------------------------------

    #[test]
    fn fleet_telemetry_adds_up_what_the_runtimes_say_and_nothing_else() {
        let python = FakeLauncher::new(SemanticBackendKind::Python);
        let rust = FakeLauncher::new(SemanticBackendKind::Rust);
        let supervisor = fleet(patient(), &[Arc::clone(&python), Arc::clone(&rust)]);
        let (release, gate) = mpsc::channel();
        *python
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(gate);

        let py = binding_of(SemanticBackendKind::Python, ResourceLanguage::Python, "py");
        let rs = binding_of(SemanticBackendKind::Rust, ResourceLanguage::Rust, "rs");

        let busy_lease = supervisor.acquire(&py).expect("start");
        let ready_lease = supervisor.acquire(&rs).expect("start");
        assert!(ask(&ready_lease, "quick").is_ok());

        python.set_behavior(Behavior::Blocked);
        let waiters: Vec<PendingRequest> = (0..3)
            .map(|_| {
                busy_lease
                    .submit(request_in(
                        &py.context.context_key(),
                        "slow",
                        Some("rev-1"),
                        RequestPriority::Interactive,
                    ))
                    .expect("submit")
            })
            .collect();
        python.await_entered(1);

        let fleet = supervisor.fleet_telemetry();
        assert_eq!(fleet.known_contexts, 2);
        assert_eq!(fleet.live_runtimes, 2);
        assert_eq!(fleet.busy, 1);
        assert_eq!(fleet.ready, 1);
        assert_eq!(fleet.active_leases, 2);
        assert_eq!(
            fleet.active_requests, 1,
            "three waiters, one execution: the fleet counts executions"
        );
        assert_eq!(fleet.dedupe_hits, 2);
        assert_eq!(fleet.starts_attempted, 2);
        assert_eq!(fleet.starts_succeeded, 2);
        assert_eq!(fleet.crashes, 0);
        assert_eq!(fleet.restart_attempts, 0);
        assert_eq!(fleet.requests_started, 2, "one quick, one slow");
        assert_eq!(fleet.requests_completed, 1);

        let _ = release.send(());
        for waiter in waiters {
            assert!(waiter.wait(RequestOptions::default()).is_ok());
        }
    }

    #[test]
    fn a_fleet_that_cannot_measure_its_memory_says_so_instead_of_saying_zero() {
        let measured = FakeLauncher::new(SemanticBackendKind::Python);
        let unmeasured = FakeLauncher::new(SemanticBackendKind::Rust);
        measured.report_rss(Some(100));
        let supervisor = fleet(patient(), &[Arc::clone(&measured), Arc::clone(&unmeasured)]);

        let _py = supervisor
            .acquire(&binding_of(
                SemanticBackendKind::Python,
                ResourceLanguage::Python,
                "py",
            ))
            .expect("start");
        let known_only = supervisor.fleet_telemetry();
        assert_eq!(known_only.known_rss_bytes, Some(100));
        assert_eq!(known_only.resource_usage_unknown, 0);

        let _rs = supervisor
            .acquire(&binding_of(
                SemanticBackendKind::Rust,
                ResourceLanguage::Rust,
                "rs",
            ))
            .expect("start");
        let mixed = supervisor.fleet_telemetry();
        assert_eq!(
            mixed.known_rss_bytes,
            Some(100),
            "the unmeasurable host was not added in as a zero"
        );
        assert_eq!(
            mixed.resource_usage_unknown, 1,
            "and the fleet says one host's memory is unknown"
        );
        assert_eq!(mixed.known_cpu_millis, None, "nothing measured CPU at all");
    }

    #[test]
    fn two_measured_hosts_are_a_sum_and_two_unmeasured_ones_are_not_a_zero() {
        let first = FakeLauncher::new(SemanticBackendKind::Python);
        let second = FakeLauncher::new(SemanticBackendKind::Rust);
        first.report_rss(Some(100));
        second.report_rss(Some(200));
        let supervisor = fleet(patient(), &[Arc::clone(&first), Arc::clone(&second)]);
        let _a = supervisor
            .acquire(&binding_of(
                SemanticBackendKind::Python,
                ResourceLanguage::Python,
                "py",
            ))
            .expect("start");
        let _b = supervisor
            .acquire(&binding_of(
                SemanticBackendKind::Rust,
                ResourceLanguage::Rust,
                "rs",
            ))
            .expect("start");
        assert_eq!(supervisor.fleet_telemetry().known_rss_bytes, Some(300));

        let blind = FakeLauncher::new(SemanticBackendKind::CSharp);
        let dark = fleet(patient(), &[Arc::clone(&blind)]);
        let _c = dark
            .acquire(&binding_of(
                SemanticBackendKind::CSharp,
                ResourceLanguage::CSharp,
                "cs",
            ))
            .expect("start");
        let nothing = dark.fleet_telemetry();
        assert_eq!(
            nothing.known_rss_bytes, None,
            "no measurement is not a measurement of none"
        );
        assert_eq!(nothing.resource_usage_unknown, 1);
    }

    #[test]
    fn reading_telemetry_never_starts_a_backend() {
        let (supervisor, launcher) = supervisor(patient());
        let key = binding().context.context_key();

        assert_eq!(supervisor.fleet_telemetry(), FleetTelemetry::default());
        assert!(supervisor.telemetry(&key).is_none());
        assert_eq!(supervisor.state(&key), RuntimeState::Cold);
        assert_eq!(supervisor.live_runtime_count(), 0);
        assert_eq!(launcher.launch_count(), 0);

        drop(supervisor.acquire(&binding()).expect("start"));
        assert_eq!(supervisor.sweep_idle(), 0, "a patient runtime stays");
        for _ in 0..5 {
            let _ = supervisor.fleet_telemetry();
        }
        assert_eq!(
            launcher.launch_count(),
            1,
            "looking at the fleet is not asking it for anything"
        );
    }

    // -----------------------------------------------------------------
    // Shutdown
    // -----------------------------------------------------------------

    #[test]
    fn shutdown_releases_a_mixed_fleet_exactly_once_and_takes_no_more_work() {
        let python = FakeLauncher::new(SemanticBackendKind::Python);
        let rust = FakeLauncher::new(SemanticBackendKind::Rust);
        let supervisor = fleet(patient(), &[Arc::clone(&python), Arc::clone(&rust)]);
        let (release, gate) = mpsc::channel();
        *python
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(gate);

        let py = binding_of(SemanticBackendKind::Python, ResourceLanguage::Python, "py");
        let rs = binding_of(SemanticBackendKind::Rust, ResourceLanguage::Rust, "rs");
        let busy = supervisor.acquire(&py).expect("start");
        let _idle = supervisor.acquire(&rs).expect("start");

        python.set_behavior(Behavior::Blocked);
        let inflight = busy
            .submit(request_in(
                &py.context.context_key(),
                "queued",
                None,
                RequestPriority::Background,
            ))
            .expect("submit");
        python.await_entered(1);

        supervisor.shutdown();

        assert!(matches!(
            supervisor.acquire(&py),
            Err(StartFailure::ShuttingDown)
        ));
        assert!(matches!(
            supervisor.acquire(&rs),
            Err(StartFailure::ShuttingDown)
        ));
        assert_eq!(supervisor.live_runtime_count(), 0);
        for key in [py.context.context_key(), rs.context.context_key()] {
            assert_eq!(
                supervisor.state(&key),
                RuntimeState::Stopped,
                "no runtime is READY behind a supervisor that is gone"
            );
        }
        assert_eq!(python.shutdown_count(), 1);
        assert_eq!(rust.shutdown_count(), 1);
        assert_eq!(python.worst_host_shutdown_count(), 1);
        assert_eq!(rust.worst_host_shutdown_count(), 1);

        // The in-flight request is released rather than deadlocked.
        let _ = release.send(());
        let outcome = inflight.wait(RequestOptions::default());
        assert!(
            outcome.is_err() || outcome.is_ok(),
            "it finished one way or the other, and did not hang"
        );

        // Shutting down twice is not shutting a host down twice.
        supervisor.shutdown();
        assert_eq!(python.worst_host_shutdown_count(), 1);
        assert_eq!(rust.worst_host_shutdown_count(), 1);
    }

    #[test]
    fn capacity_eviction_racing_shutdown_never_shuts_one_host_down_twice() {
        for round in 0..24 {
            let launcher = FakeLauncher::new(SemanticBackendKind::Python);
            let supervisor = Arc::new(fleet(capped(1), &[Arc::clone(&launcher)]));
            let first = binding_of(SemanticBackendKind::Python, ResourceLanguage::Python, "a");
            let second = binding_of(SemanticBackendKind::Python, ResourceLanguage::Python, "b");
            drop(supervisor.acquire(&first).expect("start"));

            // One thread needs the single slot; the other is closing
            // the whole fleet. Both reach for the same live host.
            let collide = Arc::new(Barrier::new(2));
            thread::scope(|scope| {
                {
                    let supervisor = Arc::clone(&supervisor);
                    let collide = Arc::clone(&collide);
                    let second = second.clone();
                    scope.spawn(move || {
                        collide.wait();
                        let _ = supervisor.acquire(&second);
                    });
                }
                collide.wait();
                supervisor.shutdown();
            });

            assert_eq!(
                launcher.worst_host_shutdown_count(),
                1,
                "round {round}: every host left exactly once"
            );
            assert_eq!(supervisor.live_runtime_count(), 0, "round {round}");
            let fleet = supervisor.fleet_telemetry();
            assert_eq!(fleet.active_requests, 0, "round {round}");
            assert_eq!(fleet.active_leases, 0, "round {round}");
        }
    }
}
