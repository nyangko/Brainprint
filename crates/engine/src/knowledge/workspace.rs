//! workspace.db durable knowledge: WorkItem, Working State, Work
//! Resource/Result/Handoff, Work Note, workspace-local Project State.
//!
//! Everything here belongs to one Workspace. Basic typed CRUD only --
//! resume planning, handoff selection, overlap coordination, and dirty
//! attribution are task 3; note promotion is task 4.

use std::path::Path;

use brainprint_core::{WorkItemId, WorkNoteId, WorkspaceId};
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::{
    KnowledgeError, KnowledgeScope, NewWorkItem, NewWorkNote, ProjectStateUpdate, PromotedItem,
    PromotedItemKind, Store, WorkHandoff, WorkItem, WorkItemSourceKind, WorkItemStatus, WorkNote,
    WorkNoteKind, WorkNoteStatus, WorkResource, WorkResourceRole, WorkResult, WorkResultStatus,
    WorkingState, WorkspaceProjectState, blob, provenance_columns, query_all, query_one,
    uid_column, uid_from_blob,
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
    s.baseline_generation_no, s.baseline_head, s.baseline_dirty_fingerprint, s.current_step, \
    s.progress_summary, s.remaining_summary, s.blocker_summary, s.owner_agent, \
    s.last_observed_workspace_revision, s.updated_at";

fn decode_working_state(row: &Row<'_>) -> Result<WorkingState, KnowledgeError> {
    Ok(WorkingState {
        work_item: uid_column(row, 0, "work_item")?,
        baseline_workspace_revision: row.get(1)?,
        baseline_generation_no: row.get(2)?,
        baseline_head: row.get(3)?,
        baseline_dirty_fingerprint: row.get(4)?,
        current_step: row.get(5)?,
        progress_summary: row.get(6)?,
        remaining_summary: row.get(7)?,
        blocker_summary: row.get(8)?,
        owner_agent: row.get(9)?,
        last_observed_workspace_revision: row.get(10)?,
        updated_at: row.get(11)?,
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
    r.result_generation_no, r.created_at";

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
        created_at: row.get(8)?,
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
    pub fn create_work_item(&self, new: &NewWorkItem) -> Result<WorkItem, KnowledgeError> {
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

    /// Any non-terminal status may move to any other; COMPLETED and
    /// ABANDONED are terminal and set `closed_at`.
    pub fn set_work_item_status(
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

    fn require_work_item(&self, uid: WorkItemId) -> Result<WorkItem, KnowledgeError> {
        self.get_work_item(uid)?.ok_or(KnowledgeError::NotFound {
            what: "work_item",
            uid: uid.to_string(),
        })
    }

    // ---- Working State (one current snapshot per WorkItem) ----

    /// Replace the WorkItem's current snapshot. `state.work_item` names the
    /// owner; `state.updated_at` is ignored and set to now.
    pub fn upsert_working_state(
        &self,
        state: &WorkingState,
    ) -> Result<WorkingState, KnowledgeError> {
        let work_item_id = self.work_item_row_id(state.work_item)?;
        self.connection.execute(
            "INSERT INTO working_state (work_item_id, baseline_workspace_revision, \
               baseline_generation_no, baseline_head, baseline_dirty_fingerprint, current_step, \
               progress_summary, remaining_summary, blocker_summary, owner_agent, \
               last_observed_workspace_revision, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
             ON CONFLICT (work_item_id) DO UPDATE SET \
               baseline_workspace_revision = excluded.baseline_workspace_revision, \
               baseline_generation_no = excluded.baseline_generation_no, \
               baseline_head = excluded.baseline_head, \
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
                state.baseline_dirty_fingerprint,
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
    pub fn record_work_resource(
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
    pub fn record_work_result(&self, result: &WorkResult) -> Result<WorkResult, KnowledgeError> {
        let work_item_id = self.work_item_row_id(result.work_item)?;
        self.connection.execute(
            "INSERT INTO work_result (work_item_id, result_status, result_summary, commit_id, \
               change_set_fingerprint, verification_summary, result_workspace_revision, \
               result_generation_no, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT (work_item_id) DO UPDATE SET \
               result_status = excluded.result_status, result_summary = excluded.result_summary, \
               commit_id = excluded.commit_id, \
               change_set_fingerprint = excluded.change_set_fingerprint, \
               verification_summary = excluded.verification_summary, \
               result_workspace_revision = excluded.result_workspace_revision, \
               result_generation_no = excluded.result_generation_no, \
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

    pub fn add_work_handoff(&self, handoff: &WorkHandoff) -> Result<(), KnowledgeError> {
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
