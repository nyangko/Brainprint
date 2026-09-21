//! `component_state` contract for the Resource inventory component
//! (#16 task 4/5, #13 task 6 §7).
//!
//! One singleton row -- [`RESOURCE_INDEX`] at scope
//! [`WORKSPACE_SCOPE_KIND`]/[`WORKSPACE_SCOPE_KEY`] -- carries whether the
//! Resource inventory is current. Two transitions exist:
//! - [`mark_current`]: a generation just published this component's rows
//!   (#16 task 4).
//! - [`mark_dirty`]: something invalidated them and they have not been
//!   recovered yet (#16 task 5). It deliberately leaves
//!   `stable_generation_id` and `basis_workspace_revision` alone: the last
//!   published generation stays available as the last valid snapshot, it
//!   just stops being described as *current*.
//!
//! `processing_state`/`freshness_state` are closed vocabularies: an
//! unrecognized stored value is a decode error, never a silent fallback.

use std::{error::Error, fmt};

use rusqlite::{Connection, OptionalExtension, params};

use crate::db;

pub const RESOURCE_INDEX: &str = "RESOURCE_INDEX";
pub const WORKSPACE_SCOPE_KIND: &str = "WORKSPACE";
/// The component covers the whole Workspace rather than one path, and the
/// column is NOT NULL.
pub const WORKSPACE_SCOPE_KEY: &str = "*";

/// `last_error_code` written when the watcher lost continuity, naming the
/// reason this component cannot be trusted until reconcile runs (#16 task
/// 6).
pub const WATCHER_CONTINUITY_LOST_CODE: &str = "WATCHER_CONTINUITY_LOST";

/// `last_error_code` written when a reconcile failed and the Resource
/// inventory therefore could not be confirmed current (#16 task 6). It is
/// only written when nothing already explains the dirty state -- an earlier
/// reason, such as [`WATCHER_CONTINUITY_LOST_CODE`], is not overwritten by
/// a later failure to recover from it.
pub const RECONCILE_FAILED_CODE: &str = "RECONCILE_FAILED";

/// `last_error_code` written when a current-source read found the file's
/// bytes no longer hashing to the persisted Resource's `content_hash`
/// (#16 task 11). The read refuses to slice a stale span, and marks the
/// component so later queries stop claiming CURRENT. Recovering is
/// reconcile's (#16 task 6) or the targeted refresh's (#16 task 13) job.
pub const SOURCE_HASH_MISMATCH_CODE: &str = "SOURCE_HASH_MISMATCH";

/// Whether work is outstanding for this component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessingState {
    /// Nothing outstanding: the component reflects a published generation.
    Ready,
    /// Work is queued for this component and has not been done yet.
    Queued,
}

impl ProcessingState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "READY",
            Self::Queued => "QUEUED",
        }
    }

    fn parse(raw: &str) -> Result<Self, ComponentError> {
        match raw {
            "READY" => Ok(Self::Ready),
            "QUEUED" => Ok(Self::Queued),
            other => Err(ComponentError::UnknownProcessingState {
                raw: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for ProcessingState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Whether this component's rows still describe the current Workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshnessState {
    Current,
    /// Known stale. The previously published generation is still readable
    /// as the last valid snapshot -- it is simply not current.
    Dirty,
}

impl FreshnessState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Current => "CURRENT",
            Self::Dirty => "DIRTY",
        }
    }

    fn parse(raw: &str) -> Result<Self, ComponentError> {
        match raw {
            "CURRENT" => Ok(Self::Current),
            "DIRTY" => Ok(Self::Dirty),
            other => Err(ComponentError::UnknownFreshnessState {
                raw: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for FreshnessState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The `RESOURCE_INDEX` row as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceIndexState {
    pub basis_workspace_revision: String,
    /// The last generation that published this component's rows. Preserved
    /// across [`mark_dirty`].
    pub stable_generation_id: Option<i64>,
    pub processing_state: ProcessingState,
    pub freshness_state: FreshnessState,
    pub last_error_code: Option<String>,
}

#[derive(Debug)]
pub enum ComponentError {
    Sqlite(rusqlite::Error),
    UnknownProcessingState { raw: String },
    UnknownFreshnessState { raw: String },
}

impl fmt::Display for ComponentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(source) => write!(formatter, "component_state sqlite error: {source}"),
            Self::UnknownProcessingState { raw } => {
                write!(formatter, "unknown processing state {raw:?}")
            }
            Self::UnknownFreshnessState { raw } => {
                write!(formatter, "unknown freshness state {raw:?}")
            }
        }
    }
}

impl Error for ComponentError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(source) => Some(source),
            Self::UnknownProcessingState { .. } | Self::UnknownFreshnessState { .. } => None,
        }
    }
}

impl From<rusqlite::Error> for ComponentError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

/// `component_state`'s columns as stored, before vocabulary decoding.
type RawComponentRow = (String, Option<i64>, String, String, Option<String>);

/// Read the `RESOURCE_INDEX` row, or `None` if it has never been written.
pub(crate) fn read(connection: &Connection) -> Result<Option<ResourceIndexState>, ComponentError> {
    let raw: Option<RawComponentRow> = connection
        .query_row(
            "SELECT basis_workspace_revision, stable_generation_id, processing_state, \
                    freshness_state, last_error_code \
             FROM component_state \
             WHERE component_kind = ?1 AND scope_kind = ?2 AND scope_key = ?3",
            params![RESOURCE_INDEX, WORKSPACE_SCOPE_KIND, WORKSPACE_SCOPE_KEY],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;

    raw.map(
        |(
            basis_workspace_revision,
            stable_generation_id,
            processing_state,
            freshness_state,
            last_error_code,
        )| {
            Ok(ResourceIndexState {
                basis_workspace_revision,
                stable_generation_id,
                processing_state: ProcessingState::parse(&processing_state)?,
                freshness_state: FreshnessState::parse(&freshness_state)?,
                last_error_code,
            })
        },
    )
    .transpose()
}

/// READY/CURRENT against `generation_id`, which just published this
/// component's rows. Clears any previous error code.
pub(crate) fn mark_current(
    connection: &Connection,
    basis_workspace_revision: &str,
    generation_id: i64,
) -> Result<(), ComponentError> {
    connection.execute(
        "INSERT INTO component_state \
         (component_kind, scope_kind, scope_key, basis_workspace_revision, \
          stable_generation_id, processing_state, freshness_state, last_error_code, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8) \
         ON CONFLICT (component_kind, scope_kind, scope_key) DO UPDATE SET \
         basis_workspace_revision = excluded.basis_workspace_revision, \
         stable_generation_id = excluded.stable_generation_id, \
         processing_state = excluded.processing_state, \
         freshness_state = excluded.freshness_state, \
         last_error_code = NULL, \
         updated_at = excluded.updated_at",
        params![
            RESOURCE_INDEX,
            WORKSPACE_SCOPE_KIND,
            WORKSPACE_SCOPE_KEY,
            basis_workspace_revision,
            generation_id,
            ProcessingState::Ready.as_str(),
            FreshnessState::Current.as_str(),
            db::now_millis_text(),
        ],
    )?;
    Ok(())
}

/// QUEUED/DIRTY, preserving `stable_generation_id` and
/// `basis_workspace_revision` so the last published generation stays
/// available as the last valid snapshot.
///
/// `fallback_basis_workspace_revision` is used only when the row does not
/// exist yet (the column is NOT NULL); an existing row keeps its own basis.
/// `error_code`, when given, records *why* -- an existing code is kept if
/// `None` is passed, since a later event does not undo an earlier failure.
pub(crate) fn mark_dirty(
    connection: &Connection,
    fallback_basis_workspace_revision: &str,
    error_code: Option<&str>,
) -> Result<(), ComponentError> {
    connection.execute(
        "INSERT INTO component_state \
         (component_kind, scope_kind, scope_key, basis_workspace_revision, \
          stable_generation_id, processing_state, freshness_state, last_error_code, updated_at) \
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, ?8) \
         ON CONFLICT (component_kind, scope_kind, scope_key) DO UPDATE SET \
         processing_state = excluded.processing_state, \
         freshness_state = excluded.freshness_state, \
         last_error_code = COALESCE(excluded.last_error_code, component_state.last_error_code), \
         updated_at = excluded.updated_at",
        params![
            RESOURCE_INDEX,
            WORKSPACE_SCOPE_KIND,
            WORKSPACE_SCOPE_KEY,
            fallback_basis_workspace_revision,
            ProcessingState::Queued.as_str(),
            FreshnessState::Dirty.as_str(),
            error_code,
            db::now_millis_text(),
        ],
    )?;
    Ok(())
}
