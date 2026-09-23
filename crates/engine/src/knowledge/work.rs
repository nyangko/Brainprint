//! Workspace-bound WorkItem lifecycle (#20 task 3).
//!
//! [`WorkRuntime`] is the production path for WorkItem / Working State /
//! Work Resource / Work Result / Work Handoff writes. It is bound to one
//! explicit Workspace: its workspace.db and index.db must both carry that
//! WorkspaceID, or it refuses to open. Nothing is bound or repaired here.
//!
//! - No "current task": every operation except [`WorkRuntime::create`]
//!   names its WorkItem, and a Workspace may hold any number of ACTIVE /
//!   BLOCKED / PAUSED items.
//! - The first OPEN → ACTIVE transition fixes the baseline (Workspace
//!   revision + stable generation read from index.db, never from the
//!   caller) in the same workspace.db transaction. Nothing rewrites it.
//! - Git HEAD, dirty state, commit IDs and verification are observations
//!   the caller supplies; this module runs no command.
//! - Only [`WorkRuntime::complete`] / [`WorkRuntime::abandon`] reach a
//!   terminal status. A commit, a result, or an old `updated_at` never do.

use std::{error::Error, fmt, path::Path};

use brainprint_core::{IndexIncarnationId, ResourceId, WorkItemId, WorkspaceId};

use super::{
    DirtyObservation, KnowledgeError, NewWorkItem, WorkHandoff, WorkItem, WorkItemStatus,
    WorkResource, WorkResourceRole, WorkResult, WorkResultStatus, WorkingState,
    WorkspaceKnowledgeStore, uid_from_blob,
};
use crate::generation::{GenerationError, GenerationRecord, GenerationState, GenerationStore};

/// Work Resources read per WorkItem. Exceeding it is an error, never a
/// truncated snapshot (continuation is task 7).
const RESOURCE_BOUND: u32 = 512;
/// Overlap role-pair rows read per query.
const OVERLAP_BOUND: u32 = 256;

const STARTED: [WorkItemStatus; 3] = [
    WorkItemStatus::Active,
    WorkItemStatus::Blocked,
    WorkItemStatus::Paused,
];

/// Why index.db cannot supply an exact baseline right now. The caller
/// retries; nothing here waits, syncs, or runs init.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotReady {
    ClockNotBootstrapped,
    NoStableGeneration,
    /// The stable generation was built for an older Workspace revision:
    /// the daemon is still catching up.
    StableBehind {
        stable_basis: String,
        current: String,
    },
}

#[derive(Debug)]
pub enum WorkError {
    Knowledge(KnowledgeError),
    Generation(GenerationError),
    /// The database file does not exist; it is never created here.
    MissingDatabase {
        db: &'static str,
    },
    /// `db_meta.workspace_uid` is NULL: init never bound this file.
    UnboundWorkspace {
        db: &'static str,
    },
    WorkspaceMismatch {
        db: &'static str,
        expected: WorkspaceId,
        found: WorkspaceId,
    },
    WorkspaceNotReady(NotReady),
    /// Supplied observations contradict each other.
    InvalidObservation(String),
    BoundExceeded {
        what: &'static str,
    },
}

impl fmt::Display for WorkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Knowledge(source) => write!(formatter, "{source}"),
            Self::Generation(source) => write!(formatter, "{source}"),
            Self::MissingDatabase { db } => write!(formatter, "{db} does not exist"),
            Self::UnboundWorkspace { db } => write!(formatter, "{db} is not bound to a Workspace"),
            Self::WorkspaceMismatch {
                db,
                expected,
                found,
            } => write!(
                formatter,
                "{db} belongs to Workspace {found}, not {expected}"
            ),
            Self::WorkspaceNotReady(reason) => {
                write!(
                    formatter,
                    "Workspace has no current stable baseline: {reason:?}"
                )
            }
            Self::InvalidObservation(reason) => write!(formatter, "invalid observation: {reason}"),
            Self::BoundExceeded { what } => write!(formatter, "{what} exceed the work bound"),
        }
    }
}

impl Error for WorkError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Knowledge(source) => Some(source),
            Self::Generation(source) => Some(source),
            _ => None,
        }
    }
}

impl From<KnowledgeError> for WorkError {
    fn from(source: KnowledgeError) -> Self {
        Self::Knowledge(source)
    }
}

impl From<GenerationError> for WorkError {
    fn from(source: GenerationError) -> Self {
        Self::Generation(source)
    }
}

impl From<rusqlite::Error> for WorkError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Knowledge(source.into())
    }
}

/// A Resource named by the caller, with an optional locator hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceObservation {
    pub resource: ResourceId,
    pub locator_hint: Option<String>,
}

/// Observed at first activation. Revision and generation are not here:
/// they are read from index.db.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartObservation {
    pub head: Option<String>,
    pub dirty: DirtyObservation,
    /// Known dirty Resources before the task; requires `dirty` DIRTY.
    pub preexisting_dirty: Vec<ResourceObservation>,
    pub owner_agent: Option<String>,
}

/// The mutable part of the Working State, replaced as a whole.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkProgress {
    pub current_step: Option<String>,
    pub progress_summary: Option<String>,
    pub remaining_summary: Option<String>,
    pub blocker_summary: Option<String>,
    /// Attribution hint only; not a session, lock, or selector.
    pub owner_agent: Option<String>,
}

/// Task-side Resource evidence. PREEXISTING_DIRTY is baseline-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceEvidence {
    pub resource: ResourceId,
    pub role: WorkResourceRole,
    pub locator_hint: Option<String>,
}

/// A result as observed by the caller. Revision and generation are read
/// from index.db.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultObservation {
    pub summary: String,
    pub commit_id: Option<String>,
    pub change_set_fingerprint: Option<String>,
    pub verification_summary: Option<String>,
    pub remaining_dirty: DirtyObservation,
}

/// Whether a stored generation value reference still names the same
/// generation in the current index.db.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationReferenceState {
    /// The stored index incarnation is the current index.db's, and it has
    /// a STABLE row with this number and this basis revision.
    PresentMatching,
    /// Anything else: another index.db incarnation (a rebuild, even one
    /// that reused the same number and revision), no incarnation stored
    /// (pre-correction row), no such row, a different basis, or not
    /// STABLE. Never rebound to the current generation.
    HistoricalMissingOrReused,
}

/// A stored generation value reference: which index.db incarnation, which
/// generation number, observed at which Workspace revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationReference {
    pub index_incarnation: Option<IndexIncarnationId>,
    pub generation_no: i64,
    pub workspace_revision: String,
    pub state: GenerationReferenceState,
}

/// Deterministic staleness against a caller-supplied cutoff; there is no
/// built-in timeout. Never changes status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Staleness {
    NotEvaluated,
    CurrentAtCutoff,
    PossiblyStale,
    Closed,
}

/// Engine-internal aggregate for one WorkItem (input to task 5; not a
/// transport or projection shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkSnapshot {
    pub workspace_id: WorkspaceId,
    pub item: WorkItem,
    pub working_state: Option<WorkingState>,
    pub resources: Vec<WorkResource>,
    pub result: Option<WorkResult>,
    pub latest_handoff: Option<WorkHandoff>,
    pub baseline_generation: Option<GenerationReference>,
    pub result_generation: Option<GenerationReference>,
    pub staleness: Staleness,
}

/// Another non-terminal WorkItem whose edit scope shares a Resource. A
/// coordination fact only: nothing is locked, assigned, or blocked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkOverlap {
    pub other: WorkItemId,
    pub other_status: WorkItemStatus,
    pub resource: ResourceId,
    pub this_roles: Vec<WorkResourceRole>,
    pub other_roles: Vec<WorkResourceRole>,
}

/// The Workspace clock as read from index.db.
struct Observed {
    incarnation: IndexIncarnationId,
    revision: String,
    stable: Option<GenerationRecord>,
}

impl Observed {
    /// The stable generation, if it was built for exactly this revision,
    /// named within this index.db incarnation.
    fn matching_generation(&self) -> Option<(IndexIncarnationId, i64)> {
        self.stable
            .as_ref()
            .filter(|stable| stable.basis_workspace_revision == self.revision)
            .map(|stable| (self.incarnation, stable.generation_no))
    }
}

pub struct WorkRuntime {
    workspace_id: WorkspaceId,
    store: WorkspaceKnowledgeStore,
    index: GenerationStore,
}

impl WorkRuntime {
    /// Open the lifecycle runtime for `workspace_id`. Both files must
    /// exist and both must be bound to exactly that Workspace.
    pub fn open(
        workspace_id: WorkspaceId,
        workspace_db: &Path,
        index_db: &Path,
    ) -> Result<Self, WorkError> {
        for (db, path) in [("workspace.db", workspace_db), ("index.db", index_db)] {
            if !path.is_file() {
                return Err(WorkError::MissingDatabase { db });
            }
        }
        let store = WorkspaceKnowledgeStore::open(workspace_db)?;
        let index = GenerationStore::open(index_db)?;
        check_binding("workspace.db", workspace_id, store.bound_workspace_id()?)?;
        let index_bound = index
            .bound_workspace_uid()?
            .map(|bytes| uid_from_blob(&bytes, "db_meta"))
            .transpose()?;
        check_binding("index.db", workspace_id, index_bound)?;
        Ok(Self {
            workspace_id,
            store,
            index,
        })
    }

    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    /// Create an OPEN WorkItem. It has no baseline until [`Self::start`].
    pub fn create(&self, new: &NewWorkItem) -> Result<WorkItem, WorkError> {
        Ok(self.store.create_work_item(new)?)
    }

    /// OPEN → ACTIVE, fixing the baseline exactly once. Requires index.db's
    /// stable generation to be built for the current Workspace revision.
    pub fn start(
        &self,
        work_item: WorkItemId,
        observation: &StartObservation,
    ) -> Result<WorkingState, WorkError> {
        observation.dirty.validate()?;
        if !observation.preexisting_dirty.is_empty()
            && !matches!(observation.dirty, DirtyObservation::Dirty { .. })
        {
            return Err(WorkError::InvalidObservation(
                "pre-existing dirty Resources need a DIRTY baseline observation".to_owned(),
            ));
        }
        if observation.preexisting_dirty.len() > RESOURCE_BOUND as usize {
            return Err(WorkError::BoundExceeded {
                what: "pre-existing dirty Resources",
            });
        }
        let item = self.require_item(work_item)?;
        if item.status != WorkItemStatus::Open {
            return Err(KnowledgeError::InvalidTransition {
                what: "work_item",
                reason: format!("{} was already started", item.status.as_str()),
            }
            .into());
        }
        let observed = self.observe()?;
        let (incarnation, generation_no) = match (&observed.stable, observed.matching_generation())
        {
            (_, Some(reference)) => reference,
            (None, None) => return Err(WorkError::WorkspaceNotReady(NotReady::NoStableGeneration)),
            (Some(stable), None) => {
                return Err(WorkError::WorkspaceNotReady(NotReady::StableBehind {
                    stable_basis: stable.basis_workspace_revision.clone(),
                    current: observed.revision,
                }));
            }
        };

        let transaction = self.store.begin()?;
        self.store.transition_work_item(
            work_item,
            &[WorkItemStatus::Open],
            WorkItemStatus::Active,
        )?;
        self.store.insert_working_state(&WorkingState {
            work_item,
            baseline_workspace_revision: observed.revision.clone(),
            baseline_index_incarnation: Some(incarnation),
            baseline_generation_no: generation_no,
            baseline_head: observation.head.clone(),
            baseline_dirty: observation.dirty.clone(),
            current_step: None,
            progress_summary: None,
            remaining_summary: None,
            blocker_summary: None,
            owner_agent: observation.owner_agent.clone(),
            last_observed_workspace_revision: observed.revision.clone(),
            updated_at: String::new(),
        })?;
        for dirty in &observation.preexisting_dirty {
            self.record_resource(
                work_item,
                &ResourceEvidence {
                    resource: dirty.resource,
                    role: WorkResourceRole::PreexistingDirty,
                    locator_hint: dirty.locator_hint.clone(),
                },
                &observed.revision,
            )?;
        }
        transaction.commit()?;
        self.require_working_state(work_item)
    }

    /// Replace the mutable snapshot and add Resource evidence. Baseline is
    /// untouched. Allowed while ACTIVE / BLOCKED / PAUSED.
    pub fn update_progress(
        &self,
        work_item: WorkItemId,
        progress: &WorkProgress,
        resources: &[ResourceEvidence],
    ) -> Result<WorkingState, WorkError> {
        if resources
            .iter()
            .any(|evidence| evidence.role == WorkResourceRole::PreexistingDirty)
        {
            return Err(WorkError::InvalidObservation(
                "PREEXISTING_DIRTY is recorded only at first activation".to_owned(),
            ));
        }
        let revision = self.observe()?.revision;
        let transaction = self.store.begin()?;
        let mut state = self.started_state(work_item)?;
        state.current_step.clone_from(&progress.current_step);
        state
            .progress_summary
            .clone_from(&progress.progress_summary);
        state
            .remaining_summary
            .clone_from(&progress.remaining_summary);
        state.blocker_summary.clone_from(&progress.blocker_summary);
        state.owner_agent.clone_from(&progress.owner_agent);
        state.last_observed_workspace_revision = revision.clone();
        self.store.update_working_progress(&state)?;
        for evidence in resources {
            self.record_resource(work_item, evidence, &revision)?;
        }
        transaction.commit()?;
        self.require_working_state(work_item)
    }

    /// ACTIVE → BLOCKED with the blocker recorded.
    pub fn block(&self, work_item: WorkItemId, blocker: &str) -> Result<WorkItem, WorkError> {
        self.move_started(
            work_item,
            &[WorkItemStatus::Active],
            WorkItemStatus::Blocked,
            Some(blocker),
        )
    }

    /// ACTIVE → PAUSED. No blocker is invented.
    pub fn pause(&self, work_item: WorkItemId) -> Result<WorkItem, WorkError> {
        self.move_started(
            work_item,
            &[WorkItemStatus::Active],
            WorkItemStatus::Paused,
            None,
        )
    }

    /// BLOCKED / PAUSED → ACTIVE on the same WorkItem and baseline.
    pub fn resume(&self, work_item: WorkItemId) -> Result<WorkItem, WorkError> {
        self.move_started(
            work_item,
            &[WorkItemStatus::Blocked, WorkItemStatus::Paused],
            WorkItemStatus::Active,
            None,
        )
    }

    /// Record a PARTIAL checkpoint result. The WorkItem keeps its status
    /// whatever the result carries (a commit included).
    pub fn record_partial(
        &self,
        work_item: WorkItemId,
        observation: &ResultObservation,
    ) -> Result<WorkResult, WorkError> {
        let observed = self.observe()?;
        let transaction = self.store.begin()?;
        self.started_state(work_item)?;
        let result = self.store.record_work_result(&result_row(
            work_item,
            WorkResultStatus::Partial,
            observation,
            &observed,
        ))?;
        transaction.commit()?;
        Ok(result)
    }

    /// Explicit completion from ACTIVE: final result, snapshot revision,
    /// COMPLETED + `closed_at`, and the handoff if given, in one
    /// transaction.
    pub fn complete(
        &self,
        work_item: WorkItemId,
        observation: &ResultObservation,
        handoff: Option<&WorkHandoff>,
    ) -> Result<WorkSnapshot, WorkError> {
        self.finish(
            work_item,
            &[WorkItemStatus::Active],
            WorkItemStatus::Completed,
            WorkResultStatus::Completed,
            observation,
            handoff,
        )
    }

    /// Explicit abandonment from any non-terminal status.
    pub fn abandon(
        &self,
        work_item: WorkItemId,
        observation: &ResultObservation,
        handoff: Option<&WorkHandoff>,
    ) -> Result<WorkSnapshot, WorkError> {
        self.finish(
            work_item,
            &[
                WorkItemStatus::Open,
                WorkItemStatus::Active,
                WorkItemStatus::Blocked,
                WorkItemStatus::Paused,
            ],
            WorkItemStatus::Abandoned,
            WorkResultStatus::Abandoned,
            observation,
            handoff,
        )
    }

    /// Add a compact handoff to a non-terminal WorkItem.
    pub fn add_handoff(&self, handoff: &WorkHandoff) -> Result<(), WorkError> {
        let transaction = self.store.begin()?;
        let item = self.require_item(handoff.work_item)?;
        if is_terminal(item.status) {
            return Err(terminal_error(&item));
        }
        self.store.add_work_handoff(handoff)?;
        transaction.commit()?;
        Ok(())
    }

    /// Exact snapshot of `work_item`. `stale_before_millis` is an explicit
    /// cutoff (Unix millis); without it staleness is not evaluated.
    pub fn snapshot(
        &self,
        work_item: WorkItemId,
        stale_before_millis: Option<u64>,
    ) -> Result<WorkSnapshot, WorkError> {
        let item = self.require_item(work_item)?;
        let working_state = self.store.get_working_state(work_item)?;
        let resources = self
            .store
            .list_work_resources(work_item, RESOURCE_BOUND + 1)?;
        if resources.len() > RESOURCE_BOUND as usize {
            return Err(WorkError::BoundExceeded {
                what: "Work Resources",
            });
        }
        let result = self.store.get_work_result(work_item)?;
        let latest_handoff = self.store.list_work_handoffs(work_item, 1)?.pop();
        let current = self.index.index_incarnation_id()?;
        let baseline_generation = working_state
            .as_ref()
            .map(|state| {
                self.reference(
                    current,
                    state.baseline_index_incarnation,
                    state.baseline_generation_no,
                    &state.baseline_workspace_revision,
                )
            })
            .transpose()?;
        let result_generation = result
            .as_ref()
            .and_then(|result| {
                result.result_generation_no.map(|number| {
                    self.reference(
                        current,
                        result.result_index_incarnation,
                        number,
                        &result.result_workspace_revision,
                    )
                })
            })
            .transpose()?;
        let last_update = working_state
            .as_ref()
            .map_or(&item.created_at, |state| &state.updated_at);
        let staleness = staleness(&item, last_update, stale_before_millis)?;
        Ok(WorkSnapshot {
            workspace_id: self.workspace_id,
            item,
            working_state,
            resources,
            result,
            latest_handoff,
            baseline_generation,
            result_generation,
            staleness,
        })
    }

    /// Other ACTIVE / BLOCKED / PAUSED WorkItems whose TARGET / TOUCHED /
    /// OWNED Resources intersect this one's, one entry per (Resource,
    /// other WorkItem). RELATED and PREEXISTING_DIRTY never count.
    pub fn overlaps(&self, work_item: WorkItemId) -> Result<Vec<WorkOverlap>, WorkError> {
        let rows = self.store.work_overlap_rows(work_item, OVERLAP_BOUND + 1)?;
        if rows.len() > OVERLAP_BOUND as usize {
            return Err(WorkError::BoundExceeded {
                what: "work overlaps",
            });
        }
        // Rows arrive ordered by (resource, other); fold their role pairs.
        let mut out: Vec<WorkOverlap> = Vec::new();
        for row in rows {
            if !out
                .last()
                .is_some_and(|last| last.resource == row.resource && last.other == row.other)
            {
                out.push(WorkOverlap {
                    other: row.other,
                    other_status: row.other_status,
                    resource: row.resource,
                    this_roles: Vec::new(),
                    other_roles: Vec::new(),
                });
            }
            let entry = out.last_mut().expect("an entry for this row exists");
            if !entry.this_roles.contains(&row.this_role) {
                entry.this_roles.push(row.this_role);
            }
            if !entry.other_roles.contains(&row.other_role) {
                entry.other_roles.push(row.other_role);
            }
        }
        Ok(out)
    }

    // ---- internals ----

    fn observe(&self) -> Result<Observed, WorkError> {
        let revision = self
            .index
            .current_workspace_revision()?
            .ok_or(WorkError::WorkspaceNotReady(NotReady::ClockNotBootstrapped))?;
        let stable = self.index.current_stable()?;
        Ok(Observed {
            incarnation: self.index.index_incarnation_id()?,
            revision,
            stable,
        })
    }

    /// Compare a stored reference with index.db. The incarnation is checked
    /// first: a number from another (or an unknown) incarnation is never
    /// looked up in this one.
    fn reference(
        &self,
        current: IndexIncarnationId,
        index_incarnation: Option<IndexIncarnationId>,
        generation_no: i64,
        workspace_revision: &str,
    ) -> Result<GenerationReference, WorkError> {
        let matching = index_incarnation == Some(current)
            && self
                .index
                .get_generation_by_no(generation_no)?
                .is_some_and(|row| {
                    row.state == GenerationState::Stable
                        && row.basis_workspace_revision == workspace_revision
                });
        Ok(GenerationReference {
            index_incarnation,
            generation_no,
            workspace_revision: workspace_revision.to_owned(),
            state: if matching {
                GenerationReferenceState::PresentMatching
            } else {
                GenerationReferenceState::HistoricalMissingOrReused
            },
        })
    }

    fn require_item(&self, work_item: WorkItemId) -> Result<WorkItem, WorkError> {
        self.store.get_work_item(work_item)?.ok_or_else(|| {
            KnowledgeError::NotFound {
                what: "work_item",
                uid: work_item.to_string(),
            }
            .into()
        })
    }

    fn require_working_state(&self, work_item: WorkItemId) -> Result<WorkingState, WorkError> {
        self.store.get_working_state(work_item)?.ok_or_else(|| {
            KnowledgeError::NotFound {
                what: "working_state",
                uid: work_item.to_string(),
            }
            .into()
        })
    }

    /// The Working State of an ACTIVE / BLOCKED / PAUSED WorkItem.
    fn started_state(&self, work_item: WorkItemId) -> Result<WorkingState, WorkError> {
        let item = self.require_item(work_item)?;
        if !STARTED.contains(&item.status) {
            return Err(KnowledgeError::InvalidTransition {
                what: "work_item",
                reason: format!("{} has no running lifecycle", item.status.as_str()),
            }
            .into());
        }
        self.require_working_state(work_item)
    }

    fn move_started(
        &self,
        work_item: WorkItemId,
        from: &[WorkItemStatus],
        next: WorkItemStatus,
        blocker: Option<&str>,
    ) -> Result<WorkItem, WorkError> {
        let revision = self.observe()?.revision;
        let transaction = self.store.begin()?;
        let mut state = self.started_state(work_item)?;
        let item = self.store.transition_work_item(work_item, from, next)?;
        if let Some(blocker) = blocker {
            state.blocker_summary = Some(blocker.to_owned());
        }
        state.last_observed_workspace_revision = revision;
        self.store.update_working_progress(&state)?;
        transaction.commit()?;
        Ok(item)
    }

    fn finish(
        &self,
        work_item: WorkItemId,
        from: &[WorkItemStatus],
        next: WorkItemStatus,
        result_status: WorkResultStatus,
        observation: &ResultObservation,
        handoff: Option<&WorkHandoff>,
    ) -> Result<WorkSnapshot, WorkError> {
        if handoff.is_some_and(|handoff| handoff.work_item != work_item) {
            return Err(WorkError::InvalidObservation(
                "handoff names a different WorkItem".to_owned(),
            ));
        }
        let observed = self.observe()?;
        let transaction = self.store.begin()?;
        self.store.record_work_result(&result_row(
            work_item,
            result_status,
            observation,
            &observed,
        ))?;
        if let Some(mut state) = self.store.get_working_state(work_item)? {
            state
                .last_observed_workspace_revision
                .clone_from(&observed.revision);
            self.store.update_working_progress(&state)?;
        }
        self.store.transition_work_item(work_item, from, next)?;
        if let Some(handoff) = handoff {
            self.store.add_work_handoff(handoff)?;
        }
        transaction.commit()?;
        self.snapshot(work_item, None)
    }

    fn record_resource(
        &self,
        work_item: WorkItemId,
        evidence: &ResourceEvidence,
        revision: &str,
    ) -> Result<(), WorkError> {
        self.store.record_work_resource(&WorkResource {
            work_item,
            resource: evidence.resource,
            role: evidence.role,
            locator_hint: evidence.locator_hint.clone(),
            first_observed_revision: revision.to_owned(),
            last_observed_revision: revision.to_owned(),
        })?;
        Ok(())
    }
}

fn check_binding(
    db: &'static str,
    expected: WorkspaceId,
    found: Option<WorkspaceId>,
) -> Result<(), WorkError> {
    match found {
        None => Err(WorkError::UnboundWorkspace { db }),
        Some(found) if found != expected => Err(WorkError::WorkspaceMismatch {
            db,
            expected,
            found,
        }),
        Some(_) => Ok(()),
    }
}

const fn is_terminal(status: WorkItemStatus) -> bool {
    matches!(
        status,
        WorkItemStatus::Completed | WorkItemStatus::Abandoned
    )
}

fn terminal_error(item: &WorkItem) -> WorkError {
    KnowledgeError::InvalidTransition {
        what: "work_item",
        reason: format!("{} is terminal", item.status.as_str()),
    }
    .into()
}

/// A result row: revision observed now; generation only if the stable one
/// was built for exactly that revision (never a last-valid older one).
fn result_row(
    work_item: WorkItemId,
    result_status: WorkResultStatus,
    observation: &ResultObservation,
    observed: &Observed,
) -> WorkResult {
    let generation = observed.matching_generation();
    WorkResult {
        work_item,
        result_status,
        result_summary: observation.summary.clone(),
        commit_id: observation.commit_id.clone(),
        change_set_fingerprint: observation.change_set_fingerprint.clone(),
        verification_summary: observation.verification_summary.clone(),
        result_workspace_revision: observed.revision.clone(),
        result_index_incarnation: generation.map(|(incarnation, _)| incarnation),
        result_generation_no: generation.map(|(_, number)| number),
        remaining_dirty: observation.remaining_dirty.clone(),
        created_at: String::new(),
    }
}

fn staleness(
    item: &WorkItem,
    last_update: &str,
    cutoff: Option<u64>,
) -> Result<Staleness, WorkError> {
    if is_terminal(item.status) {
        return Ok(Staleness::Closed);
    }
    let Some(cutoff) = cutoff else {
        return Ok(Staleness::NotEvaluated);
    };
    let updated: u64 = last_update
        .parse()
        .map_err(|_| KnowledgeError::Inconsistent {
            table: "working_state",
            reason: format!("updated_at {last_update:?} is not Unix millis"),
        })?;
    Ok(if updated < cutoff {
        Staleness::PossiblyStale
    } else {
        Staleness::CurrentAtCutoff
    })
}

#[cfg(test)]
mod tests;
