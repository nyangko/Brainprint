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
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use brainprint_core::{IndexIncarnationId, ResourceId, WorkspaceId};
use brainprint_engine::{
    config::{WorkspaceConfig, load_workspace_config},
    generation::GenerationStore,
    paths::WorkspacePaths,
    query::{Currentness, QueryIndex},
    reconcile::Reconcile,
    refresh::{RefreshOutcome, TargetedRefresh},
    registry::GlobalRegistry,
    resource::{Resource, ResourceState, ResourceStore},
    scan::BaselineScan,
    watch::{NotifyWatchSource, RawWatchEvent, WatchIngest, WatchSource, WatcherContinuity},
};
use serde::{Deserialize, Serialize};

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

/// #55: which physical index, at which proven-current point, a command
/// boundary is measured against. Only a STABLE generation whose basis is
/// the current Workspace revision is ever a basis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexBasis {
    pub index_incarnation: IndexIncarnationId,
    pub workspace_revision: String,
    pub generation_no: i64,
    pub generation_basis_revision: String,
}

/// #55: the current basis and every ACTIVE Resource's revision, taken
/// before a command runs. Identity and revision only: no source bytes,
/// paths or metadata. A command-time publication by the watcher cannot
/// hide a change from the delta, because this predates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandBaseline {
    pub basis: IndexBasis,
    pub resources: HashMap<ResourceId, String>,
}

/// In report order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceDeltaKind {
    Created,
    Updated,
    Deleted,
}

/// One Resource that differs between a command's baseline and the
/// verified basis after it. Not a claim that the command changed it:
/// anything else that changed the Workspace meanwhile is here too. A move
/// keeps its identity, so it is `Updated`; a metadata-only refresh keeps
/// its revision, so it is not here at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceDelta {
    pub resource_id: ResourceId,
    pub kind: ResourceDeltaKind,
    /// The current path; for `Deleted`, the tombstone's last path.
    pub path: String,
    /// The current revision; for `Deleted`, the deletion's.
    pub resource_revision: String,
}

/// #55: the net change from a [`CommandBaseline`] to the basis a forced
/// verified reconcile proved current after the command. Net, not a
/// history: a Resource created and deleted in between is not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostCommandRefreshReport {
    pub before: IndexBasis,
    pub after: IndexBasis,
    /// Created, Updated, Deleted; then by path, then by ResourceId.
    pub changes: Vec<ResourceDelta>,
    pub created_count: u64,
    pub updated_count: u64,
    pub deleted_count: u64,
}

impl PostCommandRefreshReport {
    #[must_use]
    pub fn total_changed(&self) -> u64 {
        self.created_count + self.updated_count + self.deleted_count
    }
}

/// Why no current basis could be proven. Detail stays here, for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandBasisError {
    /// Recovery failed or the Workspace is gone; the index is left DIRTY.
    Refresh(String),
    NotCurrent,
    NoStableGeneration,
    StableBasisMismatch,
    /// The index.db was rebuilt since the baseline: its ResourceIds and
    /// revisions are not the baseline's history.
    IndexIncarnationChanged,
}

impl fmt::Display for CommandBasisError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refresh(error) => write!(formatter, "workspace refresh failed: {error}"),
            Self::NotCurrent => formatter.write_str("the index is not current"),
            Self::NoStableGeneration => formatter.write_str("no stable generation"),
            Self::StableBasisMismatch => {
                formatter.write_str("the stable generation is behind the workspace revision")
            }
            Self::IndexIncarnationChanged => formatter.write_str("the index was rebuilt"),
        }
    }
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
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub const fn config(&self) -> &WorkspaceConfig {
        &self.config
    }

    #[must_use]
    pub fn index_db(&self) -> &Path {
        &self.index_db
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

    /// #55: the pre-command baseline -- the query freshness barrier first,
    /// then the basis it proved and the ACTIVE Resource revisions.
    pub fn command_baseline(&mut self) -> Result<CommandBaseline, CommandBasisError> {
        self.ensure_current().map_err(CommandBasisError::Refresh)?;
        let (basis, active) = self.current_basis()?;
        Ok(CommandBaseline {
            basis,
            resources: active
                .into_iter()
                .map(|resource| (resource.id, resource.resource_revision))
                .collect(),
        })
    }

    /// #55: after a command, prove currentness again without trusting the
    /// watcher to have seen the command's writes: buffered events are
    /// journaled, then a verified reconcile runs even with nothing
    /// pending and the index already CURRENT (a write that kept size and
    /// mtime, or one not delivered yet). Then the net delta from
    /// `baseline`. Starts no semantic backend; persisted semantic
    /// contributions are withdrawn as on every structural publication.
    pub fn refresh_after_command(
        &mut self,
        baseline: &CommandBaseline,
    ) -> Result<PostCommandRefreshReport, CommandBasisError> {
        self.require_present()
            .and_then(|()| self.ingest().map(|_| ()))
            .map_err(CommandBasisError::Refresh)?;
        self.settle(true).map_err(CommandBasisError::Refresh)?;
        let (after, active) = self.current_basis()?;
        if after.index_incarnation != baseline.basis.index_incarnation {
            return Err(CommandBasisError::IndexIncarnationChanged);
        }
        let store = ResourceStore::open(&self.index_db).map_err(storage)?;
        let changes = resource_delta(&baseline.resources, active, |id| {
            store.get_by_id(id).map_err(storage)
        })?;
        let count = |kind| changes.iter().filter(|change| change.kind == kind).count() as u64;
        Ok(PostCommandRefreshReport {
            before: baseline.basis.clone(),
            created_count: count(ResourceDeltaKind::Created),
            updated_count: count(ResourceDeltaKind::Updated),
            deleted_count: count(ResourceDeltaKind::Deleted),
            after,
            changes,
        })
    }

    /// #55: whether the index proves CURRENT right now; read on this
    /// worker, so no publication interleaves.
    #[must_use]
    pub fn is_current(&self) -> bool {
        self.index_current().unwrap_or(false)
    }

    /// The basis and ACTIVE inventory, only if the index proves CURRENT on
    /// a stable generation at the Workspace revision. Read on this worker,
    /// so no publication interleaves.
    fn current_basis(&self) -> Result<(IndexBasis, Vec<Resource>), CommandBasisError> {
        if !self.index_current().map_err(CommandBasisError::Refresh)? {
            return Err(CommandBasisError::NotCurrent);
        }
        let generations = GenerationStore::open(&self.index_db).map_err(storage)?;
        let stable = generations
            .current_stable()
            .map_err(storage)?
            .ok_or(CommandBasisError::NoStableGeneration)?;
        let workspace_revision = generations
            .current_workspace_revision()
            .map_err(storage)?
            .ok_or(CommandBasisError::NoStableGeneration)?;
        if stable.basis_workspace_revision != workspace_revision {
            return Err(CommandBasisError::StableBasisMismatch);
        }
        let basis = IndexBasis {
            index_incarnation: generations.index_incarnation_id().map_err(storage)?,
            workspace_revision,
            generation_no: stable.generation_no,
            generation_basis_revision: stable.basis_workspace_revision,
        };
        let active = ResourceStore::open(&self.index_db)
            .and_then(|store| store.list_active())
            .map_err(storage)?;
        Ok((basis, active))
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
                    super::semantic::withdraw_all(&self.index_db)?;
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

            super::semantic::withdraw_all(&self.index_db)?;
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

fn storage(error: impl fmt::Display) -> CommandBasisError {
    CommandBasisError::Refresh(format!("index.db: {error}"))
}

/// The net delta from `baseline` to `active`, in report order. A
/// baseline Resource no longer ACTIVE reads its tombstone through
/// `stored`.
fn resource_delta(
    baseline: &HashMap<ResourceId, String>,
    active: Vec<Resource>,
    mut stored: impl FnMut(ResourceId) -> Result<Option<Resource>, CommandBasisError>,
) -> Result<Vec<ResourceDelta>, CommandBasisError> {
    let mut changes = Vec::new();
    let mut remaining: HashMap<ResourceId, &String> = baseline
        .iter()
        .map(|(id, revision)| (*id, revision))
        .collect();
    for resource in active {
        let kind = match remaining.remove(&resource.id) {
            None => ResourceDeltaKind::Created,
            Some(revision) if *revision != resource.resource_revision => ResourceDeltaKind::Updated,
            Some(_) => continue,
        };
        changes.push(ResourceDelta {
            resource_id: resource.id,
            kind,
            path: resource.path_rel,
            resource_revision: resource.resource_revision,
        });
    }
    for id in remaining.into_keys() {
        let tombstone = stored(id)?
            .filter(|resource| resource.state == ResourceState::Deleted)
            .ok_or_else(|| {
                CommandBasisError::Refresh("a baseline Resource has no tombstone".to_owned())
            })?;
        changes.push(ResourceDelta {
            resource_id: id,
            kind: ResourceDeltaKind::Deleted,
            path: tombstone.path_rel,
            resource_revision: tombstone.resource_revision,
        });
    }
    changes.sort_by(|a, b| (a.kind, &a.path, a.resource_id).cmp(&(b.kind, &b.path, b.resource_id)));
    Ok(changes)
}
