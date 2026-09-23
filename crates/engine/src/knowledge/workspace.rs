//! workspace.db durable knowledge: WorkItem, Working State, Work
//! Resource/Result/Handoff, Work Note, workspace-local Project State.
//!
//! Everything here belongs to one Workspace. Typed storage primitives
//! only. The WorkItem lifecycle (baseline once, explicit transitions,
//! atomic finalization) is [`super::WorkRuntime`] (task 3), which is why
//! the WorkItem/Working State/Result/Handoff writers are crate-internal;
//! note promotion is task 4.

use std::path::Path;

use brainprint_core::{WorkItemId, WorkNoteId, WorkspaceId};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use super::{
    DirtyObservation, DirtyState, KnowledgeError, KnowledgeScope, NewWorkItem, NewWorkNote,
    ProjectStateUpdate, PromotedItem, PromotedItemKind, Store, WorkHandoff, WorkItem,
    WorkItemSourceKind, WorkItemStatus, WorkNote, WorkNoteKind, WorkNoteStatus, WorkResource,
    WorkResourceRole, WorkResult, WorkResultStatus, WorkingState, WorkspaceProjectState, blob,
    provenance_columns, query_all, query_one, uid_column, uid_from_blob,
};
use crate::{db, schema};

/// Handle to one Workspace's `workspace.db`.
pub struct WorkspaceKnowledgeStore {
    connection: Connection,
}

const WORK_ITEM_COLUMNS: &str =
    "uid, source_kind, source_ref, title, goal, status, created_at, closed_at";

fn decode_work_item(row: &Row<'_>) -> Result<WorkItem, KnowledgeError> {
    Ok(WorkItem {
        uid: uid_column(row, 0, "work_item")?,
        source_kind: WorkItemSourceKind::parse(&row.get::<_, String>(1)?)?,
        source_ref: row.get(2)?,
        title: row.get(3)?,
        goal: row.get(4)?,
        status: WorkItemStatus::parse(&row.get::<_, String>(5)?)?,
        created_at: row.get(6)?,
        closed_at: row.get(7)?,
    })
}

const WORKING_STATE_COLUMNS: &str = "w.uid, s.baseline_workspace_revision, \
    s.baseline_generation_no, s.baseline_head, s.baseline_dirty_state, \
    s.baseline_dirty_fingerprint, s.current_step, s.progress_summary, s.remaining_summary, \
    s.blocker_summary, s.owner_agent, s.last_observed_workspace_revision, s.updated_at";

fn dirty_columns(row: &Row<'_>, state: usize) -> Result<DirtyObservation, KnowledgeError> {
    DirtyObservation::from_parts(
        DirtyState::parse(&row.get::<_, String>(state)?)?,
        row.get(state + 1)?,
    )
}

fn decode_working_state(row: &Row<'_>) -> Result<WorkingState, KnowledgeError> {
    Ok(WorkingState {
        work_item: uid_column(row, 0, "work_item")?,
        baseline_workspace_revision: row.get(1)?,
        baseline_generation_no: row.get(2)?,
        baseline_head: row.get(3)?,
        baseline_dirty: dirty_columns(row, 4)?,
        current_step: row.get(6)?,
        progress_summary: row.get(7)?,
        remaining_summary: row.get(8)?,
        blocker_summary: row.get(9)?,
        owner_agent: row.get(10)?,
        last_observed_workspace_revision: row.get(11)?,
        updated_at: row.get(12)?,
    })
}

const WORK_RESOURCE_COLUMNS: &str = "w.uid, r.resource_uid, r.role, r.locator_hint, \
    r.first_observed_revision, r.last_observed_revision";

fn decode_work_resource(row: &Row<'_>) -> Result<WorkResource, KnowledgeError> {
    Ok(WorkResource {
        work_item: uid_column(row, 0, "work_item")?,
        resource: uid_column(row, 1, "work_resource")?,
        role: WorkResourceRole::parse(&row.get::<_, String>(2)?)?,
        locator_hint: row.get(3)?,
        first_observed_revision: row.get(4)?,
        last_observed_revision: row.get(5)?,
    })
}

const WORK_RESULT_COLUMNS: &str = "w.uid, r.result_status, r.result_summary, r.commit_id, \
    r.change_set_fingerprint, r.verification_summary, r.result_workspace_revision, \
    r.result_generation_no, r.remaining_dirty_state, r.remaining_dirty_fingerprint, r.created_at";

fn decode_work_result(row: &Row<'_>) -> Result<WorkResult, KnowledgeError> {
    Ok(WorkResult {
        work_item: uid_column(row, 0, "work_item")?,
        result_status: WorkResultStatus::parse(&row.get::<_, String>(1)?)?,
        result_summary: row.get(2)?,
        commit_id: row.get(3)?,
        change_set_fingerprint: row.get(4)?,
        verification_summary: row.get(5)?,
        result_workspace_revision: row.get(6)?,
        result_generation_no: row.get(7)?,
        remaining_dirty: dirty_columns(row, 8)?,
        created_at: row.get(10)?,
    })
}

const WORK_HANDOFF_COLUMNS: &str = "w.uid, h.handoff_summary, h.remaining_summary, \
    h.blocker_summary, h.next_scope_hint, h.created_at";

fn decode_work_handoff(row: &Row<'_>) -> Result<WorkHandoff, KnowledgeError> {
    Ok(WorkHandoff {
        work_item: uid_column(row, 0, "work_item")?,
        handoff_summary: row.get(1)?,
        remaining_summary: row.get(2)?,
        blocker_summary: row.get(3)?,
        next_scope_hint: row.get(4)?,
        created_at: row.get(5)?,
    })
}

const WORK_NOTE_COLUMNS: &str = "n.uid, w.uid, n.kind, n.note_text, n.status, n.source_kind, \
    n.source_ref, n.source_revision, n.promoted_item_kind, n.promoted_item_uid, n.created_at, \
    n.updated_at";

fn decode_work_note(row: &Row<'_>) -> Result<WorkNote, KnowledgeError> {
    let status = WorkNoteStatus::parse(&row.get::<_, String>(4)?)?;
    let promoted_kind: Option<String> = row.get(8)?;
    let promoted_uid: Option<Vec<u8>> = row.get(9)?;
    let promoted_item = match (promoted_kind, promoted_uid) {
        (None, None) => None,
        (Some(kind), Some(uid)) => Some(match PromotedItemKind::parse(&kind)? {
            PromotedItemKind::Policy => PromotedItem::Policy(uid_from_blob(&uid, "work_note")?),
            PromotedItemKind::Decision => PromotedItem::Decision(uid_from_blob(&uid, "work_note")?),
            PromotedItemKind::ProjectState => {
                PromotedItem::ProjectState(uid_from_blob(&uid, "work_note")?)
            }
        }),
        _ => {
            return Err(KnowledgeError::Inconsistent {
                table: "work_note",
                reason: "promoted kind and uid must be set together".to_owned(),
            });
        }
    };
    if (status == WorkNoteStatus::Promoted) != promoted_item.is_some() {
        return Err(KnowledgeError::Inconsistent {
            table: "work_note",
            reason: format!(
                "status {} with promoted target {promoted_item:?}",
                status.as_str()
            ),
        });
    }
    Ok(WorkNote {
        uid: uid_column(row, 0, "work_note")?,
        work_item: uid_column(row, 1, "work_item")?,
        kind: WorkNoteKind::parse(&row.get::<_, String>(2)?)?,
        note_text: row.get(3)?,
        status,
        provenance: provenance_columns(row, 5)?,
        promoted_item,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

/// T6 in the task 3 SQL access plan: driven by the requesting WorkItem's
/// own edit-scope rows, then `idx_work_resource_resource_role`.
pub(crate) const OVERLAP_SQL: &str = "SELECT o.uid, o.status, r.resource_uid, s.role, r.role \
    FROM work_resource s \
    JOIN work_resource r ON r.resource_uid = s.resource_uid AND r.work_item_id <> s.work_item_id \
    JOIN work_item o ON o.id = r.work_item_id \
    WHERE s.work_item_id = ?1 \
      AND s.role IN ('TARGET', 'TOUCHED', 'OWNED') \
      AND r.role IN ('TARGET', 'TOUCHED', 'OWNED') \
      AND o.status IN ('ACTIVE', 'BLOCKED', 'PAUSED') \
    ORDER BY r.resource_uid, o.id, s.role, r.role LIMIT ?2";

/// One role pair of an edit-scope overlap.
pub(crate) struct OverlapRow {
    pub other: WorkItemId,
    pub other_status: WorkItemStatus,
    pub resource: brainprint_core::ResourceId,
    pub this_role: WorkResourceRole,
    pub other_role: WorkResourceRole,
}

impl WorkspaceKnowledgeStore {
    pub fn open(path: &Path) -> Result<Self, KnowledgeError> {
        Ok(Self::from_connection(
            schema::workspace::open(path)?.connection,
        ))
    }

    #[must_use]
    pub fn from_connection(connection: Connection) -> Self {
        Self { connection }
    }

    /// The Workspace this workspace.db was bound to by init, if any.
    pub fn bound_workspace_id(&self) -> Result<Option<WorkspaceId>, KnowledgeError> {
        let bound: Option<Vec<u8>> = self.connection.query_row(
            "SELECT workspace_uid FROM db_meta WHERE id = 0",
            [],
            |row| row.get(0),
        )?;
        bound
            .map(|bytes| super::uid_from_blob(&bytes, "db_meta"))
            .transpose()
    }

    /// A workspace.db transaction the lifecycle runs several primitives
    /// in; every primitive uses this same connection.
    pub(crate) fn begin(&self) -> Result<Transaction<'_>, KnowledgeError> {
        Ok(self.connection.unchecked_transaction()?)
    }

    fn work_item_row_id(&self, uid: WorkItemId) -> Result<i64, KnowledgeError> {
        self.connection
            .query_row(
                "SELECT id FROM work_item WHERE uid = ?1",
                params![blob(uid)],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(KnowledgeError::NotFound {
                what: "work_item",
                uid: uid.to_string(),
            })
    }

    // ---- WorkItem ----

    /// Create a WorkItem in OPEN status.
    pub(crate) fn create_work_item(&self, new: &NewWorkItem) -> Result<WorkItem, KnowledgeError> {
        let uid = WorkItemId::generate();
        self.connection.execute(
            "INSERT INTO work_item (uid, source_kind, source_ref, title, goal, status, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                blob(uid),
                new.source_kind.as_str(),
                new.source_ref,
                new.title,
                new.goal,
                WorkItemStatus::Open.as_str(),
                db::now_millis_text(),
            ],
        )?;
        self.require_work_item(uid)
    }

    pub fn get_work_item(&self, uid: WorkItemId) -> Result<Option<WorkItem>, KnowledgeError> {
        query_one(
            &self.connection,
            &format!("SELECT {WORK_ITEM_COLUMNS} FROM work_item WHERE uid = ?1"),
            params![blob(uid)],
            decode_work_item,
        )
    }

    pub fn list_work_items(
        &self,
        status: WorkItemStatus,
        limit: u32,
    ) -> Result<Vec<WorkItem>, KnowledgeError> {
        query_all(
            &self.connection,
            &format!(
                "SELECT {WORK_ITEM_COLUMNS} FROM work_item WHERE status = ?1 ORDER BY id LIMIT ?2"
            ),
            params![status.as_str(), limit],
            decode_work_item,
        )
    }

    /// Task 1's unrestricted setter, kept for storage tests only: any
    /// non-terminal status may move to any other. The lifecycle uses
    /// [`Self::transition_work_item`].
    #[cfg(test)]
    pub(crate) fn set_work_item_status(
        &self,
        uid: WorkItemId,
        next: WorkItemStatus,
    ) -> Result<WorkItem, KnowledgeError> {
        let terminal = |status| {
            matches!(
                status,
                WorkItemStatus::Completed | WorkItemStatus::Abandoned
            )
        };
        let current = self.require_work_item(uid)?;
        if terminal(current.status) || current.status == next {
            return Err(KnowledgeError::InvalidTransition {
                what: "work_item",
                reason: format!("{} -> {}", current.status.as_str(), next.as_str()),
            });
        }
        let closed_at = terminal(next).then(db::now_millis_text);
        self.connection.execute(
            "UPDATE work_item SET status = ?1, closed_at = ?2 WHERE uid = ?3",
            params![next.as_str(), closed_at, blob(uid)],
        )?;
        self.require_work_item(uid)
    }

    /// Compare-and-set status transition: moves `uid` to `next` only if its
    /// current status is one of `from`, in one statement. A terminal
    /// `next` sets `closed_at`.
    pub(crate) fn transition_work_item(
        &self,
        uid: WorkItemId,
        from: &[WorkItemStatus],
        next: WorkItemStatus,
    ) -> Result<WorkItem, KnowledgeError> {
        let terminal = matches!(next, WorkItemStatus::Completed | WorkItemStatus::Abandoned);
        let placeholders = vec!["?"; from.len()].join(", ");
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![
            Box::new(next.as_str()),
            Box::new(terminal.then(db::now_millis_text)),
            Box::new(blob(uid)),
        ];
        values.extend(
            from.iter()
                .map(|status| Box::new(status.as_str()) as Box<dyn rusqlite::ToSql>),
        );
        let changed = self.connection.execute(
            &format!(
                "UPDATE work_item SET status = ?, closed_at = ? \
                 WHERE uid = ? AND status IN ({placeholders})"
            ),
            rusqlite::params_from_iter(values),
        )?;
        let current = self.require_work_item(uid)?;
        if changed == 0 {
            return Err(KnowledgeError::InvalidTransition {
                what: "work_item",
                reason: format!("{} -> {}", current.status.as_str(), next.as_str()),
            });
        }
        Ok(current)
    }

    fn require_work_item(&self, uid: WorkItemId) -> Result<WorkItem, KnowledgeError> {
        self.get_work_item(uid)?.ok_or(KnowledgeError::NotFound {
            what: "work_item",
            uid: uid.to_string(),
        })
    }

    // ---- Working State (one current snapshot per WorkItem) ----

    /// Task 1's whole-row replace, baseline included. Storage tests only:
    /// the lifecycle writes the baseline once ([`Self::insert_working_state`])
    /// and afterwards only the mutable part ([`Self::update_working_progress`]).
    #[cfg(test)]
    pub(crate) fn upsert_working_state(
        &self,
        state: &WorkingState,
    ) -> Result<WorkingState, KnowledgeError> {
        let work_item_id = self.work_item_row_id(state.work_item)?;
        self.connection.execute(
            "INSERT INTO working_state (work_item_id, baseline_workspace_revision, \
               baseline_generation_no, baseline_head, baseline_dirty_state, \
               baseline_dirty_fingerprint, current_step, progress_summary, remaining_summary, \
               blocker_summary, owner_agent, last_observed_workspace_revision, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
             ON CONFLICT (work_item_id) DO UPDATE SET \
               baseline_workspace_revision = excluded.baseline_workspace_revision, \
               baseline_generation_no = excluded.baseline_generation_no, \
               baseline_head = excluded.baseline_head, \
               baseline_dirty_state = excluded.baseline_dirty_state, \
               baseline_dirty_fingerprint = excluded.baseline_dirty_fingerprint, \
               current_step = excluded.current_step, \
               progress_summary = excluded.progress_summary, \
               remaining_summary = excluded.remaining_summary, \
               blocker_summary = excluded.blocker_summary, owner_agent = excluded.owner_agent, \
               last_observed_workspace_revision = excluded.last_observed_workspace_revision, \
               updated_at = excluded.updated_at",
            params![
                work_item_id,
                state.baseline_workspace_revision,
                state.baseline_generation_no,
                state.baseline_head,
                state.baseline_dirty.state().as_str(),
                state.baseline_dirty.fingerprint(),
                state.current_step,
                state.progress_summary,
                state.remaining_summary,
                state.blocker_summary,
                state.owner_agent,
                state.last_observed_workspace_revision,
                db::now_millis_text(),
            ],
        )?;
        self.get_working_state(state.work_item)?
            .ok_or(KnowledgeError::NotFound {
                what: "working_state",
                uid: state.work_item.to_string(),
            })
    }

    /// Write the WorkItem's first Working State: the baseline plus the
    /// initial mutable part. Plain INSERT -- an existing snapshot (an
    /// already fixed baseline) is a constraint error, never overwritten.
    /// `state.updated_at` is ignored.
    pub(crate) fn insert_working_state(&self, state: &WorkingState) -> Result<(), KnowledgeError> {
        state.baseline_dirty.validate()?;
        let work_item_id = self.work_item_row_id(state.work_item)?;
        self.connection.execute(
            "INSERT INTO working_state (work_item_id, baseline_workspace_revision, \
               baseline_generation_no, baseline_head, baseline_dirty_state, \
               baseline_dirty_fingerprint, current_step, progress_summary, remaining_summary, \
               blocker_summary, owner_agent, last_observed_workspace_revision, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                work_item_id,
                state.baseline_workspace_revision,
                state.baseline_generation_no,
                state.baseline_head,
                state.baseline_dirty.state().as_str(),
                state.baseline_dirty.fingerprint(),
                state.current_step,
                state.progress_summary,
                state.remaining_summary,
                state.blocker_summary,
                state.owner_agent,
                state.last_observed_workspace_revision,
                db::now_millis_text(),
            ],
        )?;
        Ok(())
    }

    /// Replace only the mutable part of an existing snapshot; the
    /// `baseline_*` columns are not in this statement at all.
    pub(crate) fn update_working_progress(
        &self,
        state: &WorkingState,
    ) -> Result<(), KnowledgeError> {
        let work_item_id = self.work_item_row_id(state.work_item)?;
        let changed = self.connection.execute(
            "UPDATE working_state SET current_step = ?2, progress_summary = ?3, \
               remaining_summary = ?4, blocker_summary = ?5, owner_agent = ?6, \
               last_observed_workspace_revision = ?7, updated_at = ?8 \
             WHERE work_item_id = ?1",
            params![
                work_item_id,
                state.current_step,
                state.progress_summary,
                state.remaining_summary,
                state.blocker_summary,
                state.owner_agent,
                state.last_observed_workspace_revision,
                db::now_millis_text(),
            ],
        )?;
        if changed == 0 {
            return Err(KnowledgeError::NotFound {
                what: "working_state",
                uid: state.work_item.to_string(),
            });
        }
        Ok(())
    }

    pub fn get_working_state(
        &self,
        work_item: WorkItemId,
    ) -> Result<Option<WorkingState>, KnowledgeError> {
        query_one(
            &self.connection,
            &format!(
                "SELECT {WORKING_STATE_COLUMNS} FROM work_item w \
                 JOIN working_state s ON s.work_item_id = w.id WHERE w.uid = ?1"
            ),
            params![blob(work_item)],
            decode_working_state,
        )
    }

    // ---- Work Resource ----

    /// Record that `resource` plays `role` in the WorkItem. Re-recording
    /// the same (resource, role) only advances `last_observed_revision`
    /// (and the locator hint). `resource` is a stable index.db ResourceID
    /// value; it need not exist in any index.db.
    pub(crate) fn record_work_resource(
        &self,
        resource: &WorkResource,
    ) -> Result<WorkResource, KnowledgeError> {
        let work_item_id = self.work_item_row_id(resource.work_item)?;
        self.connection.execute(
            "INSERT INTO work_resource (work_item_id, resource_uid, role, locator_hint, \
               first_observed_revision, last_observed_revision) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT (work_item_id, resource_uid, role) DO UPDATE SET \
               locator_hint = excluded.locator_hint, \
               last_observed_revision = excluded.last_observed_revision",
            params![
                work_item_id,
                blob(resource.resource),
                resource.role.as_str(),
                resource.locator_hint,
                resource.first_observed_revision,
                resource.last_observed_revision,
            ],
        )?;
        query_one(
            &self.connection,
            &format!(
                "SELECT {WORK_RESOURCE_COLUMNS} FROM work_resource r \
                 JOIN work_item w ON w.id = r.work_item_id \
                 WHERE r.work_item_id = ?1 AND r.resource_uid = ?2 AND r.role = ?3"
            ),
            params![
                work_item_id,
                blob(resource.resource),
                resource.role.as_str()
            ],
            decode_work_resource,
        )?
        .ok_or(KnowledgeError::NotFound {
            what: "work_resource",
            uid: resource.resource.to_string(),
        })
    }

    pub fn list_work_resources(
        &self,
        work_item: WorkItemId,
        limit: u32,
    ) -> Result<Vec<WorkResource>, KnowledgeError> {
        let work_item_id = self.work_item_row_id(work_item)?;
        query_all(
            &self.connection,
            &format!(
                "SELECT {WORK_RESOURCE_COLUMNS} FROM work_resource r \
                 JOIN work_item w ON w.id = r.work_item_id \
                 WHERE r.work_item_id = ?1 ORDER BY r.id LIMIT ?2"
            ),
            params![work_item_id, limit],
            decode_work_resource,
        )
    }

    // ---- Work Result (one per WorkItem) ----

    /// Record (or replace) the WorkItem's result. A `commit_id` never
    /// implies COMPLETED; the caller states `result_status` explicitly.
    pub(crate) fn record_work_result(
        &self,
        result: &WorkResult,
    ) -> Result<WorkResult, KnowledgeError> {
        result.remaining_dirty.validate()?;
        let work_item_id = self.work_item_row_id(result.work_item)?;
        self.connection.execute(
            "INSERT INTO work_result (work_item_id, result_status, result_summary, commit_id, \
               change_set_fingerprint, verification_summary, result_workspace_revision, \
               result_generation_no, remaining_dirty_state, remaining_dirty_fingerprint, \
               created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
             ON CONFLICT (work_item_id) DO UPDATE SET \
               result_status = excluded.result_status, result_summary = excluded.result_summary, \
               commit_id = excluded.commit_id, \
               change_set_fingerprint = excluded.change_set_fingerprint, \
               verification_summary = excluded.verification_summary, \
               result_workspace_revision = excluded.result_workspace_revision, \
               result_generation_no = excluded.result_generation_no, \
               remaining_dirty_state = excluded.remaining_dirty_state, \
               remaining_dirty_fingerprint = excluded.remaining_dirty_fingerprint, \
               created_at = excluded.created_at",
            params![
                work_item_id,
                result.result_status.as_str(),
                result.result_summary,
                result.commit_id,
                result.change_set_fingerprint,
                result.verification_summary,
                result.result_workspace_revision,
                result.result_generation_no,
                result.remaining_dirty.state().as_str(),
                result.remaining_dirty.fingerprint(),
                db::now_millis_text(),
            ],
        )?;
        self.get_work_result(result.work_item)?
            .ok_or(KnowledgeError::NotFound {
                what: "work_result",
                uid: result.work_item.to_string(),
            })
    }

    pub fn get_work_result(
        &self,
        work_item: WorkItemId,
    ) -> Result<Option<WorkResult>, KnowledgeError> {
        query_one(
            &self.connection,
            &format!(
                "SELECT {WORK_RESULT_COLUMNS} FROM work_item w \
                 JOIN work_result r ON r.work_item_id = w.id WHERE w.uid = ?1"
            ),
            params![blob(work_item)],
            decode_work_result,
        )
    }

    // ---- Work Handoff ----

    pub(crate) fn add_work_handoff(&self, handoff: &WorkHandoff) -> Result<(), KnowledgeError> {
        let work_item_id = self.work_item_row_id(handoff.work_item)?;
        self.connection.execute(
            "INSERT INTO work_handoff (work_item_id, handoff_summary, remaining_summary, \
               blocker_summary, next_scope_hint, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                work_item_id,
                handoff.handoff_summary,
                handoff.remaining_summary,
                handoff.blocker_summary,
                handoff.next_scope_hint,
                db::now_millis_text(),
            ],
        )?;
        Ok(())
    }

    /// Newest first.
    pub fn list_work_handoffs(
        &self,
        work_item: WorkItemId,
        limit: u32,
    ) -> Result<Vec<WorkHandoff>, KnowledgeError> {
        let work_item_id = self.work_item_row_id(work_item)?;
        query_all(
            &self.connection,
            &format!(
                "SELECT {WORK_HANDOFF_COLUMNS} FROM work_handoff h \
                 JOIN work_item w ON w.id = h.work_item_id \
                 WHERE h.work_item_id = ?1 ORDER BY h.id DESC LIMIT ?2"
            ),
            params![work_item_id, limit],
            decode_work_handoff,
        )
    }

    /// Edit-scope overlap rows of `work_item` with other ACTIVE / BLOCKED /
    /// PAUSED WorkItems: `(other uid, other status, resource, this role,
    /// other role)`, one row per role pair, ordered by resource, other
    /// WorkItem, roles. Only TARGET / TOUCHED / OWNED count on either side.
    /// `limit` bounds the rows read.
    pub(crate) fn work_overlap_rows(
        &self,
        work_item: WorkItemId,
        limit: u32,
    ) -> Result<Vec<OverlapRow>, KnowledgeError> {
        let work_item_id = self.work_item_row_id(work_item)?;
        let mut statement = self.connection.prepare_cached(OVERLAP_SQL)?;
        let mut rows = statement.query(params![work_item_id, limit])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(OverlapRow {
                other: uid_column(row, 0, "work_item")?,
                other_status: WorkItemStatus::parse(&row.get::<_, String>(1)?)?,
                resource: uid_column(row, 2, "work_resource")?,
                this_role: WorkResourceRole::parse(&row.get::<_, String>(3)?)?,
                other_role: WorkResourceRole::parse(&row.get::<_, String>(4)?)?,
            });
        }
        Ok(out)
    }

    // ---- Work Note ----

    /// Add an OPEN note to a WorkItem. A PROPOSAL stays a note: nothing is
    /// written to project.db.
    pub fn add_work_note(
        &self,
        work_item: WorkItemId,
        new: &NewWorkNote,
    ) -> Result<WorkNote, KnowledgeError> {
        let work_item_id = self.work_item_row_id(work_item)?;
        let uid = WorkNoteId::generate();
        self.connection.execute(
            "INSERT INTO work_note (uid, work_item_id, kind, note_text, status, source_kind, \
               source_ref, source_revision, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![
                blob(uid),
                work_item_id,
                new.kind.as_str(),
                new.note_text,
                WorkNoteStatus::Open.as_str(),
                new.provenance.source_kind.as_str(),
                new.provenance.locator,
                new.provenance.revision,
                db::now_millis_text(),
            ],
        )?;
        self.require_work_note(uid)
    }

    pub fn get_work_note(&self, uid: WorkNoteId) -> Result<Option<WorkNote>, KnowledgeError> {
        query_one(
            &self.connection,
            &format!(
                "SELECT {WORK_NOTE_COLUMNS} FROM work_note n \
                 JOIN work_item w ON w.id = n.work_item_id WHERE n.uid = ?1"
            ),
            params![blob(uid)],
            decode_work_note,
        )
    }

    /// Notes of one WorkItem, optionally narrowed by kind and/or status,
    /// oldest first.
    pub fn list_work_notes(
        &self,
        work_item: WorkItemId,
        kind: Option<WorkNoteKind>,
        status: Option<WorkNoteStatus>,
        limit: u32,
    ) -> Result<Vec<WorkNote>, KnowledgeError> {
        let work_item_id = self.work_item_row_id(work_item)?;
        let filter = match (kind, status) {
            (Some(_), Some(_)) => "AND n.kind = ?2 AND n.status = ?3",
            (Some(_), None) => "AND n.kind = ?2 AND ?3 IS NULL",
            (None, Some(_)) => "AND ?2 IS NULL AND n.status = ?3",
            (None, None) => "AND ?2 IS NULL AND ?3 IS NULL",
        };
        query_all(
            &self.connection,
            &format!(
                "SELECT {WORK_NOTE_COLUMNS} FROM work_note n \
                 JOIN work_item w ON w.id = n.work_item_id \
                 WHERE n.work_item_id = ?1 {filter} ORDER BY n.id LIMIT ?4"
            ),
            params![
                work_item_id,
                kind.map(WorkNoteKind::as_str),
                status.map(WorkNoteStatus::as_str),
                limit
            ],
            decode_work_note,
        )
    }

    /// OPEN -> RESOLVED / DISCARDED only. PROMOTED is written by task 4's
    /// promotion transaction after the target row exists, never here.
    pub fn set_work_note_status(
        &self,
        uid: WorkNoteId,
        next: WorkNoteStatus,
    ) -> Result<WorkNote, KnowledgeError> {
        let current = self.require_work_note(uid)?;
        let allowed = current.status == WorkNoteStatus::Open
            && matches!(next, WorkNoteStatus::Resolved | WorkNoteStatus::Discarded);
        if !allowed {
            return Err(KnowledgeError::InvalidTransition {
                what: "work_note",
                reason: format!("{} -> {}", current.status.as_str(), next.as_str()),
            });
        }
        self.connection.execute(
            "UPDATE work_note SET status = ?1, updated_at = ?2 WHERE uid = ?3",
            params![next.as_str(), db::now_millis_text(), blob(uid)],
        )?;
        self.require_work_note(uid)
    }

    fn require_work_note(&self, uid: WorkNoteId) -> Result<WorkNote, KnowledgeError> {
        self.get_work_note(uid)?.ok_or(KnowledgeError::NotFound {
            what: "work_note",
            uid: uid.to_string(),
        })
    }

    // ---- workspace-local Project State ----

    pub fn get_workspace_project_state(
        &self,
        key: &str,
        scope: &KnowledgeScope,
    ) -> Result<Option<WorkspaceProjectState>, KnowledgeError> {
        super::get_state(&self.connection, "workspace_project_state", key, scope)
    }

    pub fn upsert_workspace_project_state(
        &self,
        update: &ProjectStateUpdate,
    ) -> Result<WorkspaceProjectState, KnowledgeError> {
        super::upsert_state(
            &self.connection,
            "workspace_project_state",
            Store::Workspace,
            update,
        )
    }

    pub fn list_workspace_project_state(
        &self,
        scope: &KnowledgeScope,
        limit: u32,
    ) -> Result<Vec<WorkspaceProjectState>, KnowledgeError> {
        super::list_state(&self.connection, "workspace_project_state", scope, limit)
    }
}
