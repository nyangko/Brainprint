//! Filesystem watcher event ingestion, `change_journal` candidates, and
//! the DIRTY/coalesce fast path (#16 task 5).
//!
//! The watcher is a **latency** path, never truth. Nothing here decides a
//! Resource change, applies one, or publishes a generation: an event's only
//! authority is "something over there may no longer be current". Recovering
//! actual correctness is #16 task 6's reconcile.
//!
//! ## Flow
//!
//! `OS watcher event → normalize → journal candidate → RESOURCE_INDEX
//! QUEUED/DIRTY → bounded coalesce`, all of it in one transaction per
//! event, so a journalled candidate and the dirty marking can never
//! disagree. [`component::mark_dirty`] deliberately preserves
//! `stable_generation_id`: the last published generation remains readable
//! as the last valid snapshot, it simply stops being described as current.
//!
//! ## What is journalled
//!
//! `CREATE`, `MODIFY`, `DELETE`, `MOVE`, and `BULK_HINT` -- the kinds this
//! stage can actually identify. `CONFIG_CHANGE`/`DEPENDENCY_CHANGE` are
//! *not* emitted: deciding that an edit changed a project's configuration
//! or its dependency set needs more than a path and an event kind, and
//! guessing would put a fabricated classification into the journal. Every
//! other field is the same: what is known is recorded (the ACTIVE
//! Resource's id, observed size/mtime, a candidate fingerprint), and what
//! is not known stays NULL.
//!
//! ## Workspace revision
//!
//! Receiving an event never advances `current_workspace_revision`. The
//! contract stays `event → candidate journal → validation → revision
//! advance` (#4), and only the first two steps live here. Each journal row
//! records the revision it was observed *against*, so a later validation
//! can tell what the candidate was based on. No revision scheme is
//! introduced.
//!
//! ## Fast observation
//!
//! Path events are observed in [`ObservationMode::Fast`]: a file whose size
//! and mtime still match the persisted row contributes that row's recorded
//! content hash as its candidate fingerprint. That is the latency
//! trade-off, and it keeps task 3's contract exactly as stated -- a write
//! preserving both size and mtime can be missed by this path. It is not a
//! substitute for reconcile's verified observation.

use std::{collections::HashMap, error::Error, fmt, path::Path, path::PathBuf};

use brainprint_core::ResourceId;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::{
    component::{self, ComponentError},
    config::WorkspaceConfig,
    db,
    discovery::{self, DiscoveryError},
    generation::{self, GenerationError},
    identity::{self, IdentityError, MoveEvidence, ObservationMode, ObservedResource},
    resource::{Resource, ResourceError, ResourceStore},
    schema,
};

/// Default upper bound on how far back a pending candidate may be
/// coalesced into a newer one.
///
/// A *default*, not a constant of the architecture: the right debounce
/// window is benchmark-determined, so it is a knob
/// ([`WatchIngest::with_coalesce_window_ms`]) and nothing in the contract
/// depends on its value.
pub const DEFAULT_COALESCE_WINDOW_MS: u64 = 250;

/// A `change_journal.event_kind` this stage can identify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchEventKind {
    Create,
    Modify,
    Delete,
    /// Both endpoints are known, because the backend vouched for them in
    /// one event. Never inferred from a delete plus a create.
    Move,
    /// Events were lost, or the backend asked for a rescan: the whole
    /// Workspace is a candidate and only reconcile can resolve it.
    BulkHint,
}

impl WatchEventKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Create => "CREATE",
            Self::Modify => "MODIFY",
            Self::Delete => "DELETE",
            Self::Move => "MOVE",
            Self::BulkHint => "BULK_HINT",
        }
    }

    fn parse(raw: &str) -> Result<Self, WatchError> {
        match raw {
            "CREATE" => Ok(Self::Create),
            "MODIFY" => Ok(Self::Modify),
            "DELETE" => Ok(Self::Delete),
            "MOVE" => Ok(Self::Move),
            "BULK_HINT" => Ok(Self::BulkHint),
            other => Err(WatchError::UnknownEventKind {
                raw: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for WatchEventKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A journal row's own state. Ingestion never writes [`Self::Applied`];
/// only a verified reconcile does (#16 task 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalState {
    /// An outstanding candidate.
    Pending,
    /// A verified reconcile has accounted for this candidate. Its
    /// `applied_generation_id` names the generation that published the
    /// resulting Resource changes, or is NULL when the reconcile confirmed
    /// there was nothing to change (the candidate is still resolved -- it
    /// simply produced no generation).
    Applied,
    /// Superseded by a newer candidate, which `coalesced_into_seq` names.
    /// The row is kept: coalescing folds meaning forward, it never erases
    /// evidence.
    Coalesced,
    /// The path was created and deleted again without any Resource ever
    /// having existed at it -- there is no current Resource to reconcile.
    Transient,
}

impl JournalState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Applied => "APPLIED",
            Self::Coalesced => "COALESCED",
            Self::Transient => "TRANSIENT",
        }
    }

    fn parse(raw: &str) -> Result<Self, WatchError> {
        match raw {
            "PENDING" => Ok(Self::Pending),
            "APPLIED" => Ok(Self::Applied),
            "COALESCED" => Ok(Self::Coalesced),
            "TRANSIENT" => Ok(Self::Transient),
            other => Err(WatchError::UnknownJournalState {
                raw: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for JournalState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// `workspace_clock.watcher_continuity_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatcherContinuity {
    /// No watcher has reported in yet.
    Uninitialized,
    /// The event stream is believed complete.
    Continuous,
    /// Events were dropped or the backend failed. The Resource index stays
    /// DIRTY until reconcile (#16 task 6) recovers it; the fast path must
    /// not carry on as if nothing happened.
    Lost,
}

impl WatcherContinuity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Uninitialized => "UNINITIALIZED",
            Self::Continuous => "CONTINUOUS",
            Self::Lost => "LOST",
        }
    }

    fn parse(raw: &str) -> Result<Self, WatchError> {
        match raw {
            "UNINITIALIZED" => Ok(Self::Uninitialized),
            "CONTINUOUS" => Ok(Self::Continuous),
            "LOST" => Ok(Self::Lost),
            other => Err(WatchError::UnknownContinuityState {
                raw: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for WatcherContinuity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One `change_journal` row. A candidate, not a fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    pub seq: i64,
    /// The Workspace revision this candidate was observed against.
    pub workspace_revision: String,
    pub event_kind: WatchEventKind,
    /// The ACTIVE Resource at the event's *source* path, when there is
    /// one. Never invented for a path no Resource currently occupies.
    pub resource_id: Option<ResourceId>,
    pub path_before: Option<String>,
    pub path_after: Option<String>,
    pub observed_size: Option<i64>,
    pub observed_mtime_ns: Option<i64>,
    pub candidate_fingerprint: Option<String>,
    pub processing_state: JournalState,
    pub coalesced_into_seq: Option<i64>,
    pub observed_at: String,
    /// The generation that published the Resource changes this candidate
    /// was accounted for by (#16 task 6). `None` while the row is still
    /// pending, and also once a reconcile resolved it without needing to
    /// publish anything.
    pub applied_generation_id: Option<i64>,
}

/// One normalized filesystem event, as this crate defines it.
///
/// A backend's own event type never reaches the rest of Brainprint: an
/// adapter ([`WatchSource`]) translates into this closed set, so switching
/// or adding a backend cannot change the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawWatchEvent {
    Created {
        path: PathBuf,
    },
    Modified {
        path: PathBuf,
    },
    Removed {
        path: PathBuf,
    },
    /// The backend reported *both* endpoints of a rename in a single
    /// event. Only this counts as move evidence. A half-rename, or a
    /// remove and a create that merely look related, must be reported as
    /// [`Self::Removed`]/[`Self::Created`].
    RenamedPair {
        from: PathBuf,
        to: PathBuf,
    },
    /// Events were dropped, a rescan was requested, or the backend failed.
    ContinuityLost {
        detail: String,
    },
}

/// A source of normalized events. Implemented by [`NotifyWatchSource`] for
/// real filesystems and directly by tests; nothing downstream knows which.
pub trait WatchSource {
    /// Take everything observed since the last call. Never blocks.
    fn drain(&mut self) -> Vec<RawWatchEvent>;
}

/// Failure ingesting an event.
#[derive(Debug)]
pub enum WatchError {
    Discovery(DiscoveryError),
    Identity(IdentityError),
    Generation(GenerationError),
    Store(ResourceError),
    Component(ComponentError),
    UnknownEventKind { raw: String },
    UnknownJournalState { raw: String },
    UnknownContinuityState { raw: String },
}

impl fmt::Display for WatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Discovery(source) => write!(formatter, "watch path discovery failed: {source}"),
            Self::Identity(source) => write!(formatter, "watch observation failed: {source}"),
            Self::Generation(source) => write!(formatter, "workspace clock failed: {source}"),
            Self::Store(source) => write!(formatter, "change journal sqlite failed: {source}"),
            Self::Component(source) => write!(formatter, "component state failed: {source}"),
            Self::UnknownEventKind { raw } => write!(formatter, "unknown event kind {raw:?}"),
            Self::UnknownJournalState { raw } => {
                write!(formatter, "unknown journal processing state {raw:?}")
            }
            Self::UnknownContinuityState { raw } => {
                write!(formatter, "unknown watcher continuity state {raw:?}")
            }
        }
    }
}

impl Error for WatchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Discovery(source) => Some(source),
            Self::Identity(source) => Some(source),
            Self::Generation(source) => Some(source),
            Self::Store(source) => Some(source),
            Self::Component(source) => Some(source),
            Self::UnknownEventKind { .. }
            | Self::UnknownJournalState { .. }
            | Self::UnknownContinuityState { .. } => None,
        }
    }
}

impl From<DiscoveryError> for WatchError {
    fn from(source: DiscoveryError) -> Self {
        Self::Discovery(source)
    }
}

impl From<IdentityError> for WatchError {
    fn from(source: IdentityError) -> Self {
        Self::Identity(source)
    }
}

impl From<GenerationError> for WatchError {
    fn from(source: GenerationError) -> Self {
        Self::Generation(source)
    }
}

impl From<ResourceError> for WatchError {
    fn from(source: ResourceError) -> Self {
        Self::Store(source)
    }
}

impl From<ComponentError> for WatchError {
    fn from(source: ComponentError) -> Self {
        Self::Component(source)
    }
}

impl From<rusqlite::Error> for WatchError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Store(ResourceError::from(source))
    }
}

/// One Workspace's event ingestion, bound to that Workspace's `index.db`.
/// Two Workspaces are two `WatchIngest`s over two databases: an event in
/// one can never mark the other dirty.
pub struct WatchIngest {
    resources: ResourceStore,
    coalesce_window_ms: u64,
}

impl WatchIngest {
    /// Open (creating/migrating if needed) the `index.db` at `path`.
    pub fn open(path: &Path) -> Result<Self, WatchError> {
        let opened = schema::index::open(path).map_err(ResourceError::from)?;
        Ok(Self::from_connection(opened.connection))
    }

    /// Wrap an already-opened `index.db` connection.
    #[must_use]
    pub fn from_connection(connection: Connection) -> Self {
        Self {
            resources: ResourceStore::from_connection(connection),
            coalesce_window_ms: DEFAULT_COALESCE_WINDOW_MS,
        }
    }

    /// Override the coalescing window. Tuning, not contract.
    #[must_use]
    pub fn with_coalesce_window_ms(mut self, coalesce_window_ms: u64) -> Self {
        self.coalesce_window_ms = coalesce_window_ms;
        self
    }

    /// The Resource inventory this Workspace's events are observed against.
    #[must_use]
    pub fn resources(&self) -> &ResourceStore {
        &self.resources
    }

    /// Record that a watcher is running and believes its stream complete.
    pub fn mark_watcher_continuous(&self) -> Result<(), WatchError> {
        self.set_continuity(self.connection(), WatcherContinuity::Continuous)
    }

    /// The persisted continuity state.
    pub fn continuity(&self) -> Result<WatcherContinuity, WatchError> {
        continuity(self.connection())
    }

    /// Ingest one normalized event.
    ///
    /// Returns the journal row it produced, or `None` when the event is not
    /// this Workspace's business -- a path outside the root, or under an
    /// excluded directory (task 2's rules), which must never dirty the
    /// index.
    pub fn ingest(
        &self,
        workspace_root: &Path,
        config: &WorkspaceConfig,
        event: &RawWatchEvent,
    ) -> Result<Option<JournalEntry>, WatchError> {
        let transaction = self.resources.transaction()?;
        let revision = generation::current_workspace_revision(self.connection())?
            .ok_or(GenerationError::ClockNotBootstrapped)?;

        let entry = match event {
            RawWatchEvent::Created { path } => self.ingest_path(
                workspace_root,
                config,
                &revision,
                WatchEventKind::Create,
                path,
            )?,
            RawWatchEvent::Modified { path } => self.ingest_path(
                workspace_root,
                config,
                &revision,
                WatchEventKind::Modify,
                path,
            )?,
            RawWatchEvent::Removed { path } => self.ingest_path(
                workspace_root,
                config,
                &revision,
                WatchEventKind::Delete,
                path,
            )?,
            RawWatchEvent::RenamedPair { from, to } => {
                self.ingest_rename(workspace_root, config, &revision, from, to)?
            }
            RawWatchEvent::ContinuityLost { .. } => Some(self.ingest_continuity_loss(&revision)?),
        };

        let Some(entry) = entry else {
            // Nothing of ours changed, so nothing is dirtied.
            return Ok(None);
        };

        component::mark_dirty(&transaction, &revision, self.dirty_error_code(&entry))?;
        transaction.commit().map_err(ResourceError::from)?;
        Ok(Some(entry))
    }

    /// Ingest a batch in arrival order. Each event is its own transaction,
    /// so a later failure never un-dirties what an earlier event marked.
    pub fn ingest_all(
        &self,
        workspace_root: &Path,
        config: &WorkspaceConfig,
        events: &[RawWatchEvent],
    ) -> Result<Vec<JournalEntry>, WatchError> {
        let mut entries = Vec::new();
        for event in events {
            if let Some(entry) = self.ingest(workspace_root, config, event)? {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    /// Every journal row, oldest first -- including coalesced ones, which
    /// are kept as evidence.
    pub fn journal(&self) -> Result<Vec<JournalEntry>, WatchError> {
        journal(self.connection())
    }

    /// The outstanding candidates: rows still `PENDING`.
    pub fn pending_candidates(&self) -> Result<Vec<JournalEntry>, WatchError> {
        pending_candidates(self.connection())
    }

    /// Move evidence that is safe to hand to [`identity::plan_changes`]:
    /// unambiguous paired renames only. See [`pending_move_evidence`].
    pub fn pending_move_evidence(&self) -> Result<Vec<MoveEvidence>, WatchError> {
        pending_move_evidence(self.connection())
    }

    fn ingest_path(
        &self,
        workspace_root: &Path,
        config: &WorkspaceConfig,
        revision: &str,
        kind: WatchEventKind,
        path: &Path,
    ) -> Result<Option<JournalEntry>, WatchError> {
        let Some(path_rel) = workspace_relative(workspace_root, path) else {
            return Ok(None);
        };
        if discovery::is_ignored_path(workspace_root, &path_rel, config) {
            return Ok(None);
        }

        let previous = self.resources.get_active_by_path_key(&path_rel)?;
        let observed = self.observe_fast(workspace_root, &path_rel, previous.as_ref())?;
        let (path_before, path_after) = match kind {
            WatchEventKind::Create => (None, Some(path_rel.clone())),
            WatchEventKind::Delete => (Some(path_rel.clone()), None),
            _ => (Some(path_rel.clone()), Some(path_rel.clone())),
        };

        let coalesce_target = self.coalesce_target(&path_rel, kind)?;
        let effective_kind = coalesce_target.as_ref().map_or(kind, |(_, previous_kind)| {
            coalesced_kind(*previous_kind, kind).unwrap_or(kind)
        });
        let state = if matches!(
            coalesce_target.as_ref().map(|(_, kind)| *kind),
            Some(WatchEventKind::Create)
        ) && kind == WatchEventKind::Delete
            && previous.is_none()
        {
            // Created and removed again inside the window, with no Resource
            // ever at that path: there is nothing current to recover.
            JournalState::Transient
        } else {
            JournalState::Pending
        };

        let entry = self.insert_journal_row(JournalRow {
            workspace_revision: revision,
            event_kind: effective_kind,
            resource_id: previous.as_ref().map(|previous| previous.id),
            path_before,
            path_after,
            observed: observed.as_ref(),
            candidate_fingerprint: observed
                .as_ref()
                .map(|observed| identity::observed_fingerprint(observed, previous.as_ref())),
            processing_state: state,
        })?;

        // The superseded row is kept, pointed at its successor rather than
        // deleted: coalescing folds meaning forward, never evidence away.
        if let Some((superseded_seq, _)) = coalesce_target {
            self.connection().execute(
                "UPDATE change_journal SET processing_state = ?1, coalesced_into_seq = ?2 \
                 WHERE seq = ?3",
                params![JournalState::Coalesced.as_str(), entry.seq, superseded_seq],
            )?;
        }

        Ok(Some(entry))
    }

    fn ingest_rename(
        &self,
        workspace_root: &Path,
        config: &WorkspaceConfig,
        revision: &str,
        from: &Path,
        to: &Path,
    ) -> Result<Option<JournalEntry>, WatchError> {
        let from_rel = workspace_relative(workspace_root, from);
        let to_rel = workspace_relative(workspace_root, to);
        // A rename with only one endpoint inside the Workspace is a
        // delete or a create, not a move -- and the backend adapter is
        // expected to report it that way. Here, both endpoints must be
        // ours and neither may be ignored, or there is no move to record.
        let (Some(from_rel), Some(to_rel)) = (from_rel, to_rel) else {
            return Ok(None);
        };
        if discovery::is_ignored_path(workspace_root, &from_rel, config)
            || discovery::is_ignored_path(workspace_root, &to_rel, config)
        {
            return Ok(None);
        }

        let previous = self.resources.get_active_by_path_key(&from_rel)?;
        let destination = self.resources.get_active_by_path_key(&to_rel)?;
        let observed = self.observe_fast(workspace_root, &to_rel, destination.as_ref())?;

        // A MOVE row is never coalesced with anything: folding it would
        // lose one of its two endpoints.
        let entry = self.insert_journal_row(JournalRow {
            workspace_revision: revision,
            event_kind: WatchEventKind::Move,
            resource_id: previous.as_ref().map(|previous| previous.id),
            path_before: Some(from_rel),
            path_after: Some(to_rel),
            observed: observed.as_ref(),
            candidate_fingerprint: observed
                .as_ref()
                .map(|observed| identity::observed_fingerprint(observed, previous.as_ref())),
            processing_state: JournalState::Pending,
        })?;
        Ok(Some(entry))
    }

    /// A continuity loss carries no path, no Resource, and no observation:
    /// it says only that the event stream can no longer be trusted, so the
    /// whole Workspace is the candidate. The backend's own description is
    /// deliberately not persisted -- `change_journal` has no free-text
    /// column, and forcing it into a path column would make it look like
    /// evidence about a path.
    fn ingest_continuity_loss(&self, revision: &str) -> Result<JournalEntry, WatchError> {
        self.set_continuity(self.connection(), WatcherContinuity::Lost)?;
        self.insert_journal_row(JournalRow {
            workspace_revision: revision,
            event_kind: WatchEventKind::BulkHint,
            resource_id: None,
            path_before: None,
            path_after: None,
            observed: None,
            candidate_fingerprint: None,
            processing_state: JournalState::Pending,
        })
    }

    /// Observe one path on the watcher's latency path. `Ok(None)` when the
    /// path is no longer there -- the common case for a delete, and not an
    /// error.
    fn observe_fast(
        &self,
        workspace_root: &Path,
        path_rel: &str,
        previous: Option<&Resource>,
    ) -> Result<Option<ObservedResource>, WatchError> {
        let Some(discovered) = discovery::describe_path(workspace_root, path_rel)? else {
            return Ok(None);
        };
        Ok(Some(identity::observe(
            workspace_root,
            &discovered,
            previous,
            ObservationMode::Fast,
        )?))
    }

    /// The newest pending candidate for `path_rel` that this event may be
    /// folded into, if it is still inside the coalescing window and the
    /// pairing is one this stage allows.
    fn coalesce_target(
        &self,
        path_rel: &str,
        incoming: WatchEventKind,
    ) -> Result<Option<(i64, WatchEventKind)>, WatchError> {
        let row: Option<(i64, String, String)> = self
            .connection()
            .query_row(
                "SELECT seq, event_kind, observed_at FROM change_journal \
                 WHERE processing_state = ?1 \
                   AND event_kind IN ('CREATE', 'MODIFY', 'DELETE') \
                   AND (path_after = ?2 OR path_before = ?2) \
                 ORDER BY seq DESC LIMIT 1",
                params![JournalState::Pending.as_str(), path_rel],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;

        let Some((seq, kind, observed_at)) = row else {
            return Ok(None);
        };
        let kind = WatchEventKind::parse(&kind)?;
        if coalesced_kind(kind, incoming).is_none() {
            return Ok(None);
        }
        if !self.within_window(&observed_at) {
            return Ok(None);
        }
        Ok(Some((seq, kind)))
    }

    fn within_window(&self, observed_at: &str) -> bool {
        let (Ok(observed), Ok(now)) = (
            observed_at.parse::<u64>(),
            db::now_millis_text().parse::<u64>(),
        ) else {
            // An unparsable timestamp is not evidence of recency.
            return false;
        };
        now.saturating_sub(observed) <= self.coalesce_window_ms
    }

    fn insert_journal_row(&self, row: JournalRow<'_>) -> Result<JournalEntry, WatchError> {
        let observed_at = db::now_millis_text();
        self.connection().execute(
            "INSERT INTO change_journal \
             (workspace_revision, event_kind, resource_uid, path_before, path_after, \
              observed_size, observed_mtime_ns, candidate_fingerprint, processing_state, \
              coalesced_into_seq, observed_at, applied_generation_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, NULL)",
            params![
                row.workspace_revision,
                row.event_kind.as_str(),
                row.resource_id.map(|id| id.to_bytes().to_vec()),
                row.path_before,
                row.path_after,
                row.observed.map(|observed| observed.size_bytes),
                row.observed.map(|observed| observed.mtime_ns),
                row.candidate_fingerprint,
                row.processing_state.as_str(),
                observed_at,
            ],
        )?;

        Ok(JournalEntry {
            seq: self.connection().last_insert_rowid(),
            workspace_revision: row.workspace_revision.to_owned(),
            event_kind: row.event_kind,
            resource_id: row.resource_id,
            path_before: row.path_before,
            path_after: row.path_after,
            observed_size: row.observed.map(|observed| observed.size_bytes),
            observed_mtime_ns: row.observed.map(|observed| observed.mtime_ns),
            candidate_fingerprint: row.candidate_fingerprint,
            processing_state: row.processing_state,
            coalesced_into_seq: None,
            observed_at,
            applied_generation_id: None,
        })
    }

    fn dirty_error_code(&self, entry: &JournalEntry) -> Option<&'static str> {
        (entry.event_kind == WatchEventKind::BulkHint)
            .then_some(component::WATCHER_CONTINUITY_LOST_CODE)
    }

    fn set_continuity(
        &self,
        connection: &Connection,
        state: WatcherContinuity,
    ) -> Result<(), WatchError> {
        let changed = connection.execute(
            "UPDATE workspace_clock SET watcher_continuity_state = ?1 WHERE id = 0",
            params![state.as_str()],
        )?;
        if changed == 0 {
            return Err(WatchError::Generation(
                GenerationError::ClockNotBootstrapped,
            ));
        }
        Ok(())
    }

    fn connection(&self) -> &Connection {
        self.resources.connection()
    }
}

/// The values one journal insert needs, so the insert does not take ten
/// positional arguments.
struct JournalRow<'a> {
    workspace_revision: &'a str,
    event_kind: WatchEventKind,
    resource_id: Option<ResourceId>,
    path_before: Option<String>,
    path_after: Option<String>,
    observed: Option<&'a ObservedResource>,
    candidate_fingerprint: Option<String>,
    processing_state: JournalState,
}

// The `&Connection` functions below are what [`WatchIngest`]'s read methods
// are built from. They exist so reconcile (#16 task 6) can read the journal
// and the continuity state through this module's contract -- including its
// closed vocabularies and its ambiguity rules for move evidence -- inside
// its own transaction, instead of re-implementing any of it against raw SQL.

pub(crate) fn continuity(connection: &Connection) -> Result<WatcherContinuity, WatchError> {
    let raw: Option<String> = connection
        .query_row(
            "SELECT watcher_continuity_state FROM workspace_clock WHERE id = 0",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match raw {
        Some(raw) => WatcherContinuity::parse(&raw),
        None => Err(WatchError::Generation(
            GenerationError::ClockNotBootstrapped,
        )),
    }
}

pub(crate) fn journal(connection: &Connection) -> Result<Vec<JournalEntry>, WatchError> {
    query_journal(
        connection,
        "SELECT {COLUMNS} FROM change_journal ORDER BY seq",
    )
}

pub(crate) fn pending_candidates(connection: &Connection) -> Result<Vec<JournalEntry>, WatchError> {
    query_journal(
        connection,
        "SELECT {COLUMNS} FROM change_journal WHERE processing_state = 'PENDING' ORDER BY seq",
    )
}

/// The highest `change_journal.seq` written so far, or 0 for an empty
/// journal. Reconcile snapshots this before planning so it can tell, at
/// publication time, whether the watcher saw anything new meanwhile.
pub(crate) fn max_journal_seq(connection: &Connection) -> Result<i64, WatchError> {
    Ok(connection.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM change_journal",
        [],
        |row| row.get(0),
    )?)
}

/// Move evidence that is safe to hand to [`identity::plan_changes`].
///
/// Only paired-rename rows qualify at all, and among those this drops
/// anything ambiguous: an endpoint named by more than one rename, or a
/// rename whose destination is another rename's source (an `A→B`, `B→A`
/// swap, or a longer cycle). Those rows stay in the journal with both paths
/// intact -- the evidence is preserved, it is simply not turned into an
/// identity decision.
pub(crate) fn pending_move_evidence(
    connection: &Connection,
) -> Result<Vec<MoveEvidence>, WatchError> {
    let renames: Vec<(String, String)> = pending_candidates(connection)?
        .into_iter()
        .filter(|entry| entry.event_kind == WatchEventKind::Move)
        .filter_map(|entry| Some((entry.path_before?, entry.path_after?)))
        .collect();

    let mut source_uses: HashMap<&str, usize> = HashMap::new();
    let mut destination_uses: HashMap<&str, usize> = HashMap::new();
    for (from, to) in &renames {
        *source_uses.entry(from.as_str()).or_default() += 1;
        *destination_uses.entry(to.as_str()).or_default() += 1;
    }

    Ok(renames
        .iter()
        .filter(|(from, to)| {
            source_uses.get(from.as_str()) == Some(&1)
                && destination_uses.get(to.as_str()) == Some(&1)
                && !source_uses.contains_key(to.as_str())
                && !destination_uses.contains_key(from.as_str())
        })
        .map(|(from, to)| MoveEvidence {
            from_path_key: from.clone(),
            to_path_key: to.clone(),
        })
        .collect())
}

/// Mark every still-`PENDING` row up to and including `through_seq` as
/// [`JournalState::Applied`], attributed to `generation_id` when the
/// reconcile published one.
///
/// Rows are never deleted, and terminal rows (`COALESCED`, `TRANSIENT`,
/// already `APPLIED`) are left exactly as they are: their evidence and
/// their `coalesced_into_seq` links stay intact. Rows newer than
/// `through_seq` stay `PENDING`, because this reconcile did not account for
/// them.
pub(crate) fn mark_applied(
    connection: &Connection,
    through_seq: i64,
    generation_id: Option<i64>,
) -> Result<(), WatchError> {
    connection.execute(
        "UPDATE change_journal SET processing_state = ?1, applied_generation_id = ?2 \
         WHERE processing_state = ?3 AND seq <= ?4",
        params![
            JournalState::Applied.as_str(),
            generation_id,
            JournalState::Pending.as_str(),
            through_seq
        ],
    )?;
    Ok(())
}

fn query_journal(connection: &Connection, sql: &str) -> Result<Vec<JournalEntry>, WatchError> {
    const COLUMNS: &str = "seq, workspace_revision, event_kind, resource_uid, path_before, \
                           path_after, observed_size, observed_mtime_ns, \
                           candidate_fingerprint, processing_state, coalesced_into_seq, \
                           observed_at, applied_generation_id";
    let mut statement = connection.prepare(&sql.replace("{COLUMNS}", COLUMNS))?;
    let rows = statement.query_map([], raw_journal_row)?;
    rows.map(|raw| decode_journal_row(raw?)).collect()
}

/// Which consecutive pairs fold into one effective candidate, and what that
/// candidate means.
///
/// `None` means "do not coalesce". The important `None` is
/// `DELETE` → `CREATE`: a new file at a freed path is not the deleted
/// Resource, and folding the two would be exactly the false merge #16 task
/// 3 forbids. `MOVE` and `BULK_HINT` never take part at all.
fn coalesced_kind(previous: WatchEventKind, incoming: WatchEventKind) -> Option<WatchEventKind> {
    use WatchEventKind::{Create, Delete, Modify};

    match (previous, incoming) {
        // A file created then written is still, in effect, a new file.
        (Create, Modify) => Some(Create),
        (Modify, Modify) => Some(Modify),
        // A removal supersedes whatever was pending for that path.
        (Create, Delete) | (Modify, Delete) => Some(Delete),
        _ => None,
    }
}

/// `path` as a Workspace-root-relative, `/`-separated path, or `None` if it
/// is not inside the Workspace. The canonical form of the root is tried as
/// a fallback, since a backend may report either (macOS `/var` vs
/// `/private/var`).
fn workspace_relative(workspace_root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(workspace_root).ok().or_else(|| {
        let canonical = workspace_root.canonicalize().ok()?;
        path.strip_prefix(canonical).ok()
    })?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    Some(
        relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

type RawJournalRow = (
    i64,
    String,
    String,
    Option<Vec<u8>>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<String>,
    String,
    Option<i64>,
    String,
    Option<i64>,
);

fn raw_journal_row(row: &Row<'_>) -> rusqlite::Result<RawJournalRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
    ))
}

fn decode_journal_row(raw: RawJournalRow) -> Result<JournalEntry, WatchError> {
    Ok(JournalEntry {
        seq: raw.0,
        workspace_revision: raw.1,
        event_kind: WatchEventKind::parse(&raw.2)?,
        resource_id: raw.3.map(|bytes| {
            let array: [u8; 16] = bytes
                .as_slice()
                .try_into()
                .expect("change_journal.resource_uid must hold a 16-byte value");
            ResourceId::from_bytes(array)
        }),
        path_before: raw.4,
        path_after: raw.5,
        observed_size: raw.6,
        observed_mtime_ns: raw.7,
        candidate_fingerprint: raw.8,
        processing_state: JournalState::parse(&raw.9)?,
        coalesced_into_seq: raw.10,
        observed_at: raw.11,
        applied_generation_id: raw.12,
    })
}

pub use backend::NotifyWatchSource;

/// The one place a third-party watcher's types are allowed to appear.
mod backend {
    use std::{
        path::Path,
        sync::{Arc, Mutex},
    };

    use notify::{
        Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
        event::{ModifyKind, RenameMode},
    };

    use super::{RawWatchEvent, WatchSource};

    /// A [`WatchSource`] backed by `notify`'s per-platform watcher
    /// (FSEvents/inotify/ReadDirectoryChangesW), so Tier-1 coverage comes
    /// from a maintained cross-platform implementation rather than three
    /// hand-written OS bindings.
    ///
    /// `notify`'s types stop here: everything downstream sees only
    /// [`RawWatchEvent`].
    pub struct NotifyWatchSource {
        // Held so the watcher keeps running; dropping it stops the watch.
        _watcher: RecommendedWatcher,
        inbox: Arc<Mutex<Vec<RawWatchEvent>>>,
    }

    impl NotifyWatchSource {
        /// Start watching `workspace_root` recursively.
        pub fn watch(workspace_root: &Path) -> Result<Self, notify::Error> {
            let inbox: Arc<Mutex<Vec<RawWatchEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&inbox);
            let mut watcher = notify::recommended_watcher(move |result| {
                let translated = translate(result);
                if let Ok(mut inbox) = sink.lock() {
                    inbox.extend(translated);
                }
            })?;
            watcher.watch(workspace_root, RecursiveMode::Recursive)?;
            Ok(Self {
                _watcher: watcher,
                inbox,
            })
        }
    }

    impl WatchSource for NotifyWatchSource {
        fn drain(&mut self) -> Vec<RawWatchEvent> {
            match self.inbox.lock() {
                Ok(mut inbox) => std::mem::take(&mut *inbox),
                // A poisoned mutex means the callback panicked, so the
                // stream can no longer be trusted.
                Err(_) => vec![RawWatchEvent::ContinuityLost {
                    detail: "watcher callback panicked".to_owned(),
                }],
            }
        }
    }

    /// Translate one backend callback result into canonical events.
    ///
    /// Only `RenameMode::Both` becomes a [`RawWatchEvent::RenamedPair`]:
    /// that is the one shape where the backend itself vouches for both
    /// endpoints. A half-rename (`From`/`To` delivered separately) is
    /// reported as a removal or a creation, which is what it is as far as
    /// this Workspace can prove -- correlating the two halves is #16 task
    /// 6/13's problem, and guessing here would be exactly the delete-plus-
    /// create merge that is forbidden.
    pub(super) fn translate(result: Result<Event, notify::Error>) -> Vec<RawWatchEvent> {
        let event = match result {
            Ok(event) => event,
            Err(error) => {
                return vec![RawWatchEvent::ContinuityLost {
                    detail: error.to_string(),
                }];
            }
        };

        if event.need_rescan() {
            return vec![RawWatchEvent::ContinuityLost {
                detail: "backend requested a rescan: events may have been dropped".to_owned(),
            }];
        }

        match event.kind {
            EventKind::Create(_) => paths(&event, |path| RawWatchEvent::Created { path }),
            EventKind::Remove(_) => paths(&event, |path| RawWatchEvent::Removed { path }),
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => match event.paths.as_slice() {
                [from, to] => vec![RawWatchEvent::RenamedPair {
                    from: from.clone(),
                    to: to.clone(),
                }],
                // "Both" without exactly two paths is not paired evidence.
                other => other
                    .iter()
                    .map(|path| RawWatchEvent::Modified { path: path.clone() })
                    .collect(),
            },
            EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                paths(&event, |path| RawWatchEvent::Removed { path })
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                paths(&event, |path| RawWatchEvent::Created { path })
            }
            EventKind::Modify(_) => paths(&event, |path| RawWatchEvent::Modified { path }),
            // Access and backend meta-events change nothing.
            EventKind::Access(_) | EventKind::Any | EventKind::Other => Vec::new(),
        }
    }

    fn paths(
        event: &Event,
        make: impl Fn(std::path::PathBuf) -> RawWatchEvent,
    ) -> Vec<RawWatchEvent> {
        event.paths.iter().cloned().map(make).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs, process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use notify::{
        Event, EventKind,
        event::{CreateKind, ModifyKind, RenameMode},
    };

    use super::{backend::translate, *};
    use crate::{
        component::{FreshnessState, ProcessingState},
        scan::BaselineScan,
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    const INITIAL_REVISION: &str = "workspace-rev-1";

    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-watch-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            fs::create_dir_all(&root).expect("workspace root should be creatable");
            Self { base, root }
        }

        fn db_path(&self) -> PathBuf {
            self.base.join("data").join("index.db")
        }

        fn write(&self, rel: &str, contents: &str) {
            let full = self.root.join(rel);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).expect("parent dirs should be creatable");
            }
            fs::write(full, contents).expect("fixture file should be writable");
        }

        fn remove(&self, rel: &str) {
            fs::remove_file(self.root.join(rel)).expect("fixture file should be removable");
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }

        /// Publishes a baseline so the clock exists and Resources are
        /// current, then hands back an ingest bound to the same index.db.
        fn with_baseline(&self) -> (BaselineScan, WatchIngest) {
            let scan = BaselineScan::open(&self.db_path()).expect("index.db should open");
            scan.run_initial_scan(&self.root, &WorkspaceConfig::default(), INITIAL_REVISION)
                .expect("baseline scan should publish");
            let ingest = WatchIngest::open(&self.db_path()).expect("index.db should open");
            (scan, ingest)
        }

        fn ingest_events(
            &self,
            ingest: &WatchIngest,
            events: &[RawWatchEvent],
        ) -> Vec<JournalEntry> {
            ingest
                .ingest_all(&self.root, &WorkspaceConfig::default(), events)
                .expect("ingestion should succeed")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn component_state(scan: &BaselineScan) -> crate::component::ResourceIndexState {
        scan.resource_index_state()
            .expect("component state should be readable")
            .expect("component state should exist")
    }

    #[test]
    fn a_modify_event_journals_a_candidate_and_marks_the_index_dirty() {
        let fixture = Fixture::create("modify");
        fixture.write("src/lib.rs", "fn main() {}");
        let (scan, ingest) = fixture.with_baseline();
        let resource_id = ingest
            .resources()
            .get_active_by_path_key("src/lib.rs")
            .expect("lookup ok")
            .expect("resource exists")
            .id;
        assert_eq!(
            component_state(&scan).freshness_state,
            FreshnessState::Current
        );

        fixture.write("src/lib.rs", "fn main() { let edited = 1; }");
        let entries = fixture.ingest_events(
            &ingest,
            &[RawWatchEvent::Modified {
                path: fixture.path("src/lib.rs"),
            }],
        );

        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.event_kind, WatchEventKind::Modify);
        assert_eq!(entry.processing_state, JournalState::Pending);
        assert_eq!(entry.resource_id, Some(resource_id));
        assert_eq!(entry.path_before.as_deref(), Some("src/lib.rs"));
        assert_eq!(entry.path_after.as_deref(), Some("src/lib.rs"));
        assert_eq!(entry.workspace_revision, INITIAL_REVISION);
        assert!(entry.candidate_fingerprint.is_some());
        assert!(entry.observed_size.is_some());

        let state = component_state(&scan);
        assert_eq!(state.processing_state, ProcessingState::Queued);
        assert_eq!(state.freshness_state, FreshnessState::Dirty);
    }

    #[test]
    fn marking_dirty_preserves_the_previous_stable_generation_pointer() {
        let fixture = Fixture::create("preserve-stable");
        fixture.write("src/lib.rs", "fn main() {}");
        let (scan, ingest) = fixture.with_baseline();
        let published = scan
            .current_stable()
            .expect("current ok")
            .expect("stable exists");
        let before = component_state(&scan);

        fixture.write("src/lib.rs", "fn main() { let edited = 1; }");
        fixture.ingest_events(
            &ingest,
            &[RawWatchEvent::Modified {
                path: fixture.path("src/lib.rs"),
            }],
        );

        let after = component_state(&scan);
        assert_eq!(
            after.stable_generation_id,
            Some(published.id),
            "the last valid snapshot must stay reachable, just not current"
        );
        assert_eq!(
            after.basis_workspace_revision,
            before.basis_workspace_revision
        );
        assert_eq!(
            scan.current_stable()
                .expect("current ok")
                .expect("stable exists")
                .id,
            published.id
        );
    }

    #[test]
    fn raw_events_change_neither_resources_nor_the_workspace_revision() {
        let fixture = Fixture::create("no-truth");
        fixture.write("src/lib.rs", "fn main() {}");
        let (scan, ingest) = fixture.with_baseline();
        let resources_before = ingest.resources().list().expect("list ok");
        let stable_before = scan.current_stable().expect("current ok");

        fixture.write("src/lib.rs", "fn main() { let edited = 1; }");
        fixture.write("src/added.rs", "fn added() {}");
        fixture.ingest_events(
            &ingest,
            &[
                RawWatchEvent::Modified {
                    path: fixture.path("src/lib.rs"),
                },
                RawWatchEvent::Created {
                    path: fixture.path("src/added.rs"),
                },
            ],
        );

        assert_eq!(
            ingest.resources().list().expect("list ok"),
            resources_before,
            "a raw event must never decide a Resource change"
        );
        assert_eq!(
            scan.current_stable().expect("current ok"),
            stable_before,
            "a raw event must never publish or retire a generation"
        );
        assert_eq!(
            generation::current_workspace_revision(ingest.connection())
                .expect("clock read ok")
                .expect("clock exists"),
            INITIAL_REVISION,
            "receiving an event must not advance the workspace revision"
        );
    }

    #[test]
    fn repeated_modifies_coalesce_into_one_effective_candidate() {
        let fixture = Fixture::create("coalesce-modify");
        fixture.write("src/lib.rs", "fn main() {}");
        let (_scan, ingest) = fixture.with_baseline();

        let path = fixture.path("src/lib.rs");
        let entries = fixture.ingest_events(
            &ingest,
            &[
                RawWatchEvent::Modified { path: path.clone() },
                RawWatchEvent::Modified { path: path.clone() },
                RawWatchEvent::Modified { path },
            ],
        );

        let pending = ingest.pending_candidates().expect("pending ok");
        assert_eq!(pending.len(), 1, "only the newest candidate stays pending");
        assert_eq!(pending[0].seq, entries[2].seq);
        assert_eq!(pending[0].event_kind, WatchEventKind::Modify);

        let journal = ingest.journal().expect("journal ok");
        assert_eq!(journal.len(), 3, "coalescing must not delete evidence");
        assert_eq!(journal[0].processing_state, JournalState::Coalesced);
        assert_eq!(journal[0].coalesced_into_seq, Some(entries[1].seq));
        assert_eq!(journal[1].coalesced_into_seq, Some(entries[2].seq));
    }

    #[test]
    fn a_create_followed_by_a_modify_stays_a_create() {
        let fixture = Fixture::create("coalesce-create-modify");
        let (_scan, ingest) = fixture.with_baseline();
        fixture.write("src/added.rs", "fn added() {}");

        let path = fixture.path("src/added.rs");
        fixture.ingest_events(
            &ingest,
            &[
                RawWatchEvent::Created { path: path.clone() },
                RawWatchEvent::Modified { path },
            ],
        );

        let pending = ingest.pending_candidates().expect("pending ok");
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].event_kind,
            WatchEventKind::Create,
            "a file written right after creation is still a creation"
        );
    }

    #[test]
    fn a_create_then_delete_is_recorded_as_a_transient_candidate() {
        let fixture = Fixture::create("coalesce-create-delete");
        let (_scan, ingest) = fixture.with_baseline();
        fixture.write("src/temp.rs", "fn temp() {}");
        let path = fixture.path("src/temp.rs");

        let created =
            fixture.ingest_events(&ingest, &[RawWatchEvent::Created { path: path.clone() }]);
        fixture.remove("src/temp.rs");
        let deleted = fixture.ingest_events(&ingest, &[RawWatchEvent::Removed { path }]);

        assert_eq!(deleted[0].event_kind, WatchEventKind::Delete);
        assert_eq!(
            deleted[0].processing_state,
            JournalState::Transient,
            "no Resource ever existed at that path, so nothing current is affected"
        );
        assert_eq!(deleted[0].resource_id, None);

        let journal = ingest.journal().expect("journal ok");
        let create_row = journal
            .iter()
            .find(|entry| entry.seq == created[0].seq)
            .expect("the create row must be kept");
        assert_eq!(create_row.processing_state, JournalState::Coalesced);
        assert_eq!(create_row.coalesced_into_seq, Some(deleted[0].seq));
        assert!(
            ingest.pending_candidates().expect("pending ok").is_empty(),
            "a transient pair leaves nothing outstanding"
        );
    }

    #[test]
    fn a_delete_then_create_is_never_folded_into_one_identity() {
        let fixture = Fixture::create("delete-create");
        fixture.write("src/lib.rs", "fn main() {}");
        let (_scan, ingest) = fixture.with_baseline();
        let path = fixture.path("src/lib.rs");

        fixture.remove("src/lib.rs");
        let deleted =
            fixture.ingest_events(&ingest, &[RawWatchEvent::Removed { path: path.clone() }]);
        fixture.write("src/lib.rs", "totally unrelated content");
        let created = fixture.ingest_events(&ingest, &[RawWatchEvent::Created { path }]);

        let pending = ingest.pending_candidates().expect("pending ok");
        assert_eq!(
            pending.len(),
            2,
            "a new file at a freed path is its own candidate, not the old one resurrected"
        );
        assert_eq!(pending[0].seq, deleted[0].seq);
        assert_eq!(pending[0].event_kind, WatchEventKind::Delete);
        assert_eq!(pending[1].seq, created[0].seq);
        assert_eq!(pending[1].event_kind, WatchEventKind::Create);
        assert!(
            pending
                .iter()
                .all(|entry| entry.coalesced_into_seq.is_none())
        );
        assert!(
            ingest
                .pending_move_evidence()
                .expect("evidence ok")
                .is_empty(),
            "a delete plus a create is never move evidence"
        );
    }

    #[test]
    fn a_paired_rename_preserves_both_endpoints_as_move_evidence() {
        let fixture = Fixture::create("rename-pair");
        fixture.write("src/lib.rs", "fn main() {}");
        let (_scan, ingest) = fixture.with_baseline();
        let original = ingest
            .resources()
            .get_active_by_path_key("src/lib.rs")
            .expect("lookup ok")
            .expect("exists")
            .id;

        fs::rename(fixture.path("src/lib.rs"), fixture.path("src/renamed.rs"))
            .expect("rename should succeed");
        let entries = fixture.ingest_events(
            &ingest,
            &[RawWatchEvent::RenamedPair {
                from: fixture.path("src/lib.rs"),
                to: fixture.path("src/renamed.rs"),
            }],
        );

        assert_eq!(entries[0].event_kind, WatchEventKind::Move);
        assert_eq!(entries[0].path_before.as_deref(), Some("src/lib.rs"));
        assert_eq!(entries[0].path_after.as_deref(), Some("src/renamed.rs"));
        assert_eq!(entries[0].resource_id, Some(original));

        assert_eq!(
            ingest.pending_move_evidence().expect("evidence ok"),
            vec![MoveEvidence {
                from_path_key: "src/lib.rs".to_owned(),
                to_path_key: "src/renamed.rs".to_owned(),
            }]
        );
    }

    #[test]
    fn an_ambiguous_rename_cycle_is_preserved_but_not_turned_into_evidence() {
        let fixture = Fixture::create("rename-cycle");
        fixture.write("a.rs", "fn a() {}");
        fixture.write("b.rs", "fn b() {}");
        let (_scan, ingest) = fixture.with_baseline();

        // An A<->B swap: each half is individually paired, but applying
        // both would be a cycle.
        fixture.ingest_events(
            &ingest,
            &[
                RawWatchEvent::RenamedPair {
                    from: fixture.path("a.rs"),
                    to: fixture.path("b.rs"),
                },
                RawWatchEvent::RenamedPair {
                    from: fixture.path("b.rs"),
                    to: fixture.path("a.rs"),
                },
            ],
        );

        assert!(
            ingest
                .pending_move_evidence()
                .expect("evidence ok")
                .is_empty(),
            "a rename cycle must not be applied as identity-preserving moves"
        );
        let renames: Vec<_> = ingest
            .journal()
            .expect("journal ok")
            .into_iter()
            .filter(|entry| entry.event_kind == WatchEventKind::Move)
            .collect();
        assert_eq!(renames.len(), 2, "both halves must be preserved");
        for entry in &renames {
            assert!(entry.path_before.is_some() && entry.path_after.is_some());
        }
    }

    #[test]
    fn a_backend_failure_loses_continuity_and_leaves_the_index_dirty() {
        let fixture = Fixture::create("continuity-lost");
        fixture.write("src/lib.rs", "fn main() {}");
        let (scan, ingest) = fixture.with_baseline();
        ingest
            .mark_watcher_continuous()
            .expect("continuity mark ok");
        assert_eq!(
            ingest.continuity().expect("continuity ok"),
            WatcherContinuity::Continuous
        );

        let entries = fixture.ingest_events(
            &ingest,
            &[RawWatchEvent::ContinuityLost {
                detail: "inotify queue overflow".to_owned(),
            }],
        );

        assert_eq!(entries[0].event_kind, WatchEventKind::BulkHint);
        assert_eq!(entries[0].path_before, None);
        assert_eq!(entries[0].path_after, None);
        assert_eq!(
            ingest.continuity().expect("continuity ok"),
            WatcherContinuity::Lost
        );

        let state = component_state(&scan);
        assert_eq!(state.freshness_state, FreshnessState::Dirty);
        assert_eq!(state.processing_state, ProcessingState::Queued);
        assert_eq!(
            state.last_error_code.as_deref(),
            Some(component::WATCHER_CONTINUITY_LOST_CODE),
            "the reason reconcile is required must be explicit"
        );
    }

    #[test]
    fn pending_candidates_and_dirty_state_survive_a_reopen() {
        let fixture = Fixture::create("reopen");
        fixture.write("src/lib.rs", "fn main() {}");
        let pending_before;
        {
            let (_scan, ingest) = fixture.with_baseline();
            fixture.write("src/lib.rs", "fn main() { let edited = 1; }");
            fixture.ingest_events(
                &ingest,
                &[RawWatchEvent::Modified {
                    path: fixture.path("src/lib.rs"),
                }],
            );
            pending_before = ingest.pending_candidates().expect("pending ok");
        }

        let reopened = WatchIngest::open(&fixture.db_path()).expect("reopen ok");
        assert_eq!(
            reopened.pending_candidates().expect("pending ok"),
            pending_before
        );

        let scan = BaselineScan::open(&fixture.db_path()).expect("reopen ok");
        assert_eq!(
            component_state(&scan).freshness_state,
            FreshnessState::Dirty,
            "a restart must not silently forget that the index is stale"
        );
    }

    #[test]
    fn one_workspaces_events_never_dirty_another() {
        let first = Fixture::create("isolation-a");
        let second = Fixture::create("isolation-b");
        first.write("src/lib.rs", "fn main() {}");
        second.write("src/lib.rs", "fn main() {}");
        let (first_scan, first_ingest) = first.with_baseline();
        let (second_scan, _second_ingest) = second.with_baseline();

        first.write("src/lib.rs", "fn main() { let edited = 1; }");
        first.ingest_events(
            &first_ingest,
            &[RawWatchEvent::Modified {
                path: first.path("src/lib.rs"),
            }],
        );

        assert_eq!(
            component_state(&first_scan).freshness_state,
            FreshnessState::Dirty
        );
        assert_eq!(
            component_state(&second_scan).freshness_state,
            FreshnessState::Current,
            "another Workspace's index.db must be untouched"
        );
    }

    #[test]
    fn events_from_excluded_directories_are_ignored_entirely() {
        let fixture = Fixture::create("excluded");
        fixture.write("src/lib.rs", "fn main() {}");
        let (scan, ingest) = fixture.with_baseline();
        fixture.write(".git/index", "junk");
        fixture.write("node_modules/pkg/index.js", "junk");

        let entries = fixture.ingest_events(
            &ingest,
            &[
                RawWatchEvent::Modified {
                    path: fixture.path(".git/index"),
                },
                RawWatchEvent::Created {
                    path: fixture.path("node_modules/pkg/index.js"),
                },
                RawWatchEvent::Modified {
                    path: fixture.base.join("outside.rs"),
                },
            ],
        );

        assert!(entries.is_empty());
        assert!(ingest.journal().expect("journal ok").is_empty());
        assert_eq!(
            component_state(&scan).freshness_state,
            FreshnessState::Current,
            "noise from excluded paths must never dirty the index"
        );
    }

    #[test]
    fn the_fast_path_reuses_recorded_evidence_for_metadata_identical_writes() {
        let fixture = Fixture::create("fast-path");
        fixture.write("src/lib.rs", "fn main() { a() }");
        let (_scan, ingest) = fixture.with_baseline();
        let before = ingest
            .resources()
            .get_active_by_path_key("src/lib.rs")
            .expect("lookup ok")
            .expect("exists");

        // Same length, mtime restored: metadata-identical, by construction.
        let path = fixture.path("src/lib.rs");
        let mtime = fs::metadata(&path)
            .expect("metadata")
            .modified()
            .expect("mtime");
        fs::write(&path, "fn main() { b() }").expect("rewrite");
        fs::File::options()
            .write(true)
            .open(&path)
            .expect("open")
            .set_modified(mtime)
            .expect("mtime restore");

        let entries = fixture.ingest_events(&ingest, &[RawWatchEvent::Modified { path }]);

        assert_eq!(
            entries[0].candidate_fingerprint.as_deref(),
            Some(before.fingerprint.as_str()),
            "the Fast path reuses recorded evidence and can miss this edit -- \
             that contract is unchanged, and reconcile is what closes it"
        );
        assert_eq!(
            component_state(&_scan).freshness_state,
            FreshnessState::Dirty,
            "the scope is still marked not-current regardless of what Fast concluded"
        );
    }

    #[test]
    fn no_source_text_reaches_the_change_journal() {
        let fixture = Fixture::create("no-source-text");
        let secret = "fn unmistakable_journal_source_marker() {}";
        fixture.write("src/lib.rs", "fn main() {}");
        let (_scan, ingest) = fixture.with_baseline();
        fixture.write("src/lib.rs", secret);

        fixture.ingest_events(
            &ingest,
            &[RawWatchEvent::Modified {
                path: fixture.path("src/lib.rs"),
            }],
        );

        for entry in ingest.journal().expect("journal ok") {
            assert!(
                !entry
                    .candidate_fingerprint
                    .as_deref()
                    .is_some_and(|value| value.contains(secret))
            );
        }
        let raw = fs::read(fixture.db_path()).expect("index.db readable");
        assert!(
            !raw.windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "no source text may be mirrored into index.db"
        );
    }

    #[test]
    fn an_unknown_stored_vocabulary_value_is_rejected_explicitly() {
        let fixture = Fixture::create("unknown-vocabulary");
        fixture.write("src/lib.rs", "fn main() {}");
        let (_scan, ingest) = fixture.with_baseline();
        fixture.ingest_events(
            &ingest,
            &[RawWatchEvent::Modified {
                path: fixture.path("src/lib.rs"),
            }],
        );

        ingest
            .connection()
            .execute("UPDATE change_journal SET event_kind = 'BOGUS'", [])
            .expect("raw override ok");
        let error = ingest
            .journal()
            .expect_err("an unrecognized event kind must not silently decode");
        assert!(matches!(
            error,
            WatchError::UnknownEventKind { raw } if raw == "BOGUS"
        ));

        ingest
            .connection()
            .execute(
                "UPDATE workspace_clock SET watcher_continuity_state = 'BOGUS' WHERE id = 0",
                [],
            )
            .expect("raw override ok");
        assert!(matches!(
            ingest
                .continuity()
                .expect_err("an unrecognized continuity state must not silently decode"),
            WatchError::UnknownContinuityState { raw } if raw == "BOGUS"
        ));
    }

    #[test]
    fn backend_events_translate_into_the_canonical_contract() {
        let path = PathBuf::from("/workspace/src/lib.rs");
        let other = PathBuf::from("/workspace/src/renamed.rs");

        let created = translate(Ok(
            Event::new(EventKind::Create(CreateKind::File)).add_path(path.clone())
        ));
        assert_eq!(created, vec![RawWatchEvent::Created { path: path.clone() }]);

        let paired = translate(Ok(Event::new(EventKind::Modify(ModifyKind::Name(
            RenameMode::Both,
        )))
        .add_path(path.clone())
        .add_path(other.clone())));
        assert_eq!(
            paired,
            vec![RawWatchEvent::RenamedPair {
                from: path.clone(),
                to: other,
            }],
            "only a backend-paired rename may become move evidence"
        );

        let half_from = translate(Ok(Event::new(EventKind::Modify(ModifyKind::Name(
            RenameMode::From,
        )))
        .add_path(path.clone())));
        assert_eq!(
            half_from,
            vec![RawWatchEvent::Removed { path: path.clone() }],
            "a half-rename is a removal, never half of an inferred move"
        );

        let ignored = translate(Ok(Event::new(EventKind::Access(
            notify::event::AccessKind::Read,
        ))
        .add_path(path)));
        assert!(ignored.is_empty());
    }

    #[test]
    fn the_notify_backend_starts_and_stops_cleanly() {
        let fixture = Fixture::create("notify-backend");
        fixture.write("src/lib.rs", "fn main() {}");

        let mut source =
            NotifyWatchSource::watch(&fixture.root).expect("a Tier-1 backend should start");
        // Draining before anything happens must not block or fail; event
        // *delivery* timing is the OS's business, not this contract's.
        let _ = source.drain();
        drop(source);
    }
}
