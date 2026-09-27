//! Product structural lifecycle of one Workspace (#38).
//!
//! Connects the already-implemented I2 primitives -- `BaselineScan`,
//! `WatchIngest`, `TargetedRefresh`, `Reconcile` -- to the daemon. Owned
//! by that Workspace's single Task 11 worker thread (`runtime.rs`), so
//! every refresh/reconcile publication is serialized with that
//! Workspace's queries: a query can never interleave with a publication,
//! and there is exactly one watcher/ingest/refresh path per Workspace no
//! matter how many clients use it.
//!
//! Nothing here decides truth. The watcher only journals candidates and
//! marks DIRTY (`WatchIngest`); `TargetedRefresh`/`Reconcile` verify the
//! filesystem and publish. Whenever currentness cannot be proven the
//! index is left (or forced) DIRTY and the query surface reports its own
//! typed NOT_CURRENT -- never a stale generation as current.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use brainprint_core::WorkspaceId;
use brainprint_engine::{
    config::{WorkspaceConfig, load_workspace_config},
    paths::WorkspacePaths,
    query::{Currentness, QueryIndex},
    reconcile::Reconcile,
    refresh::{RefreshOutcome, TargetedRefresh},
    registry::GlobalRegistry,
    scan::BaselineScan,
    watch::{NotifyWatchSource, RawWatchEvent, WatchIngest, WatchSource, WatcherContinuity},
};

/// #38 §2: the initial Workspace revision, used only when no
/// `workspace_clock` exists yet ("baseline before the first confirmed
/// input change"). `BaselineScan` never overwrites an existing clock;
/// later revisions are the existing reconcile sequence.
pub const INITIAL_WORKSPACE_REVISION: &str = "0";

/// Upper bound on refresh/reconcile rounds one settle may run when events
/// keep arriving during recovery. A loop bound, not a debounce window:
/// exhausting it leaves the index DIRTY, which the query reports.
const MAX_SETTLE_ROUNDS: usize = 4;

/// Builds the watch source for one Workspace root. Production uses
/// [`notify_watch_factory`]; the seam exists so acceptance tests can
/// simulate an unavailable or lossy watcher deterministically.
pub type WatchFactory = Arc<dyn Fn(&Path) -> Result<Box<dyn WatchSource>, String> + Send + Sync>;

#[must_use]
pub fn notify_watch_factory() -> WatchFactory {
    Arc::new(|root: &Path| {
        NotifyWatchSource::watch(root)
            .map(|source| Box::new(source) as Box<dyn WatchSource>)
            .map_err(|error| error.to_string())
    })
}

/// Factual counters for acceptance measurement. Never a health score.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LifecycleStats {
    pub activated: bool,
    pub watcher_attached: bool,
    pub watcher_error: Option<String>,
    pub watcher_attach_attempts: u64,
    pub baseline_scans: u64,
    pub reconciles: u64,
    pub reconcile_publications: u64,
    pub refresh_publications: u64,
    pub refresh_no_ops: u64,
    pub refresh_deferrals: u64,
    pub journal_entries_ingested: u64,
    pub settle_failures: u64,
}

pub struct WorkspaceLifecycle {
    /// The registry locator this lifecycle was loaded for; a repaired
    /// locator (directory move) requires a re-bind.
    locator: PathBuf,
    root: PathBuf,
    config: WorkspaceConfig,
    index_db: PathBuf,
    factory: WatchFactory,
    watcher: Option<Box<dyn WatchSource>>,
    ingest: Option<WatchIngest>,
    stats: LifecycleStats,
    /// Journal rows ingested by the latest background tick; a batch is
    /// settled only once a tick brings nothing new (coalescing, #38 §8).
    ingested_last_tick: bool,
}

impl WorkspaceLifecycle {
    /// Resolve the Workspace's canonical root and config from the
    /// registry. The root is canonicalized because platform watchers
    /// report canonical paths; an event outside the (non-canonical) root
    /// would otherwise be dropped instead of dirtying the index.
    pub fn load(
        global_db: &Path,
        workspace: WorkspaceId,
        factory: WatchFactory,
    ) -> Result<Self, String> {
        let entry = GlobalRegistry::open(global_db)
            .and_then(|registry| registry.get_workspace(workspace))
            .map_err(|error| format!("workspace registry: {error}"))?
            .ok_or_else(|| "workspace is not registered".to_owned())?;
        let root = entry
            .locator
            .canonicalize()
            .map_err(|error| format!("workspace root {}: {error}", entry.locator.display()))?;
        let paths = WorkspacePaths::from_root(&entry.locator);
        if !paths.index_db.is_file() {
            return Err(format!("{} is missing", paths.index_db.display()));
        }
        let config =
            load_workspace_config(&paths).map_err(|error| format!("workspace config: {error}"))?;
        Ok(Self {
            locator: entry.locator,
            root,
            config,
            index_db: paths.index_db,
            factory,
            watcher: None,
            ingest: None,
            stats: LifecycleStats::default(),
            ingested_last_tick: false,
        })
    }

    /// Whether the registry now names a different locator than this
    /// lifecycle was loaded for (e.g. `init` repaired it after a move).
    #[must_use]
    pub fn locator_changed(&self, global_db: &Path, workspace: WorkspaceId) -> bool {
        GlobalRegistry::open(global_db)
            .and_then(|registry| registry.get_workspace(workspace))
            .ok()
            .flatten()
            .is_none_or(|entry| entry.locator != self.locator)
    }

    #[must_use]
    pub fn stats(&self) -> LifecycleStats {
        self.stats.clone()
    }

    #[must_use]
    pub const fn is_activated(&self) -> bool {
        self.stats.activated
    }

    /// #38 §5 activation ordering: watcher registration first (its inbox
    /// buffers events from this moment on), then baseline (no stable
    /// generation) or verified reconcile (a stable generation from before
    /// this daemon process can no longer be trusted as current), then the
    /// events buffered meanwhile are journaled and settled before any
    /// current claim is made.
    pub fn activate(&mut self) -> Result<(), String> {
        self.require_present()?;
        if self.stats.activated {
            return self.settle(false);
        }
        self.attach_watcher();

        let baseline =
            BaselineScan::open(&self.index_db).map_err(|error| format!("index.db: {error}"))?;
        let fresh = baseline
            .current_stable()
            .map_err(|error| format!("stable generation: {error}"))?
            .is_none();
        if fresh {
            baseline
                .run_initial_scan(&self.root, &self.config, INITIAL_WORKSPACE_REVISION)
                .map_err(|error| format!("initial structural scan: {error}"))?;
            self.stats.baseline_scans += 1;
        }
        drop(baseline);

        let ingest =
            WatchIngest::open(&self.index_db).map_err(|error| format!("index.db: {error}"))?;
        if self.watcher.is_some() {
            ingest
                .mark_watcher_continuous()
                .map_err(|error| format!("watcher continuity: {error}"))?;
        }
        self.ingest = Some(ingest);
        self.stats.activated = true;
        // A fresh baseline was just verified against the filesystem; a
        // pre-existing stable generation was not.
        self.settle(!fresh)
    }

    /// The query freshness barrier (#38 §9): prove currentness, or refresh
    /// / reconcile until it is proven, or leave the index DIRTY so the
    /// query reports NOT_CURRENT.
    pub fn ensure_current(&mut self) -> Result<(), String> {
        if !self.stats.activated {
            return self.activate();
        }
        self.require_present()?;
        self.settle(false)
    }

    /// The engine's `open` creates a missing `index.db`; the runtime must
    /// never resurrect a moved/deleted Workspace's data that way.
    fn require_present(&self) -> Result<(), String> {
        if self.root.is_dir() && self.index_db.is_file() {
            Ok(())
        } else {
            Err(format!(
                "workspace root or index.db is missing at {}",
                self.root.display()
            ))
        }
    }

    /// Background coalescing tick: journal what arrived; settle a batch
    /// only once it stopped growing.
    pub fn tick(&mut self) {
        if !self.stats.activated || self.watcher.is_none() || self.require_present().is_err() {
            // Degraded mode reconciles at query time only.
            return;
        }
        match self.drain() {
            // The batch stopped growing: one settle for all of it.
            Ok(0) if self.ingested_last_tick => {
                self.ingested_last_tick = false;
                let _ = self.settle(false);
            }
            Ok(0) => {}
            Ok(_) => self.ingested_last_tick = true,
            Err(error) => self.fail(&error),
        }
    }

    fn attach_watcher(&mut self) {
        self.watcher = None;
        self.stats.watcher_attach_attempts += 1;
        match (self.factory)(&self.root) {
            Ok(source) => {
                self.watcher = Some(source);
                self.stats.watcher_attached = true;
                self.stats.watcher_error = None;
            }
            Err(error) => {
                self.stats.watcher_attached = false;
                self.stats.watcher_error = Some(error);
            }
        }
    }

    fn ingest(&self) -> Result<&WatchIngest, String> {
        self.ingest
            .as_ref()
            .ok_or_else(|| "workspace runtime is not activated".to_owned())
    }

    /// Journal every buffered watcher event; returns the journal rows
    /// produced (events outside the Workspace/excluded produce none).
    fn drain(&mut self) -> Result<usize, String> {
        let Some(watcher) = self.watcher.as_mut() else {
            return Ok(0);
        };
        let events = watcher.drain();
        if events.is_empty() {
            return Ok(0);
        }
        let ingest = self
            .ingest
            .as_ref()
            .ok_or("workspace runtime is not activated")?;
        let entries = ingest
            .ingest_all(&self.root, &self.config, &events)
            .map_err(|error| format!("watch ingest: {error}"))?;
        self.stats.journal_entries_ingested += entries.len() as u64;
        Ok(entries.len())
    }

    fn index_current(&self) -> Result<bool, String> {
        let index =
            QueryIndex::open(&self.index_db).map_err(|error| format!("index.db: {error}"))?;
        Ok(index
            .currentness()
            .map_err(|error| format!("currentness: {error}"))?
            == Currentness::Current)
    }

    fn settle(&mut self, force_reconcile: bool) -> Result<(), String> {
        let result = self.settle_rounds(force_reconcile);
        if let Err(error) = &result {
            self.fail(error);
        }
        result
    }

    fn settle_rounds(&mut self, mut force_reconcile: bool) -> Result<(), String> {
        for _ in 0..MAX_SETTLE_ROUNDS {
            self.drain()?;
            let continuity = self
                .ingest()?
                .continuity()
                .map_err(|error| format!("continuity: {error}"))?;
            if self.watcher.is_some() && continuity == WatcherContinuity::Lost {
                // #38 §5 again: re-attach the watcher *before* the
                // reconcile that re-establishes currentness.
                self.attach_watcher();
                if self.watcher.is_some() {
                    self.ingest()?
                        .mark_watcher_continuous()
                        .map_err(|error| format!("watcher continuity: {error}"))?;
                }
                force_reconcile = true;
            } else if self.watcher.is_none() {
                // #38 §6: no watcher, no continuity claim -- every
                // current-dependent use gets a verified reconcile.
                self.attach_watcher();
                if self.watcher.is_some() {
                    self.ingest()?
                        .mark_watcher_continuous()
                        .map_err(|error| format!("watcher continuity: {error}"))?;
                }
                force_reconcile = true;
            }

            if !force_reconcile {
                let pending = self
                    .ingest()?
                    .pending_candidates()
                    .map_err(|error| format!("journal: {error}"))?;
                if pending.is_empty() {
                    if self.index_current()? {
                        return Ok(());
                    }
                    // Nothing pending yet not current (e.g. a recovery
                    // failure left it DIRTY): fall through to reconcile.
                } else {
                    let refresh = TargetedRefresh::open(&self.index_db)
                        .map_err(|error| format!("index.db: {error}"))?;
                    match refresh
                        .run(&self.root, &self.config)
                        .map_err(|error| format!("targeted refresh: {error}"))?
                    {
                        RefreshOutcome::Published(_) => self.stats.refresh_publications += 1,
                        RefreshOutcome::NoOp { .. } => self.stats.refresh_no_ops += 1,
                        RefreshOutcome::Deferred(_) | RefreshOutcome::Obsolete(_) => {
                            self.stats.refresh_deferrals += 1;
                            force_reconcile = true;
                        }
                    }
                    continue;
                }
            }

            let reconcile =
                Reconcile::open(&self.index_db).map_err(|error| format!("index.db: {error}"))?;
            let report = reconcile
                .run(&self.root, &self.config)
                .map_err(|error| format!("reconcile: {error}"))?;
            self.stats.reconciles += 1;
            if report.published() {
                self.stats.reconcile_publications += 1;
            }
            force_reconcile = false;
            if self.watcher.is_none() {
                // Degraded: nothing further can be drained; the verified
                // reconcile is the currentness proof.
                return Ok(());
            }
        }
        // Still busy after the bounded rounds: the index keeps whatever
        // DIRTY state the last round left and the query reports it.
        Ok(())
    }

    /// Recovery failed: make sure the index is not left claiming CURRENT,
    /// using the existing I2 vocabulary for "the event stream cannot be
    /// trusted" (journals the evidence, marks DIRTY, continuity LOST).
    fn fail(&mut self, error: &str) {
        self.stats.settle_failures += 1;
        if let Some(ingest) = &self.ingest {
            let _ = ingest.ingest(
                &self.root,
                &self.config,
                &RawWatchEvent::ContinuityLost {
                    detail: format!("brainprintd recovery failed: {error}"),
                },
            );
        }
    }
}
