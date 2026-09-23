//! project.db durable knowledge: Policy, Decision, project-local
//! Blueprint, Blueprint Application, Project State.

use std::path::Path;

use brainprint_core::{BlueprintApplicationId, BlueprintId, DecisionId, PolicyId, ProjectId};
use rusqlite::{Connection, Row, params};

use super::{
    Blueprint, BlueprintApplication, BlueprintApplicationStatus, BlueprintOwnerKind, BlueprintRef,
    BlueprintStatus, DECISION, Decision, DecisionLineage, DecisionLink, DecisionLinkKind,
    DecisionStatus, KnowledgeError, KnowledgeScope, NewBlueprint, NewBlueprintApplication,
    NewDecision, NewPolicy, PROJECT_POLICY, Policy, PolicyLineage, PolicyStatus, ProjectState,
    ProjectStateUpdate, Store, blob, provenance_columns, query_all, query_one, scope_columns,
    uid_column, uid_from_blob,
};
use crate::{db, paths::WorkspacePaths, registry::GlobalRegistry, schema};

/// Handle to one Project's project-home `project.db`.
pub struct ProjectKnowledgeStore {
    connection: Connection,
}

const DECISION_COLUMNS: &str = "uid, scope_kind, scope_key, topic, chosen_summary, rationale, \
    status, source_kind, source_locator, source_revision, created_at, updated_at";

fn decode_decision(row: &Row<'_>) -> Result<Decision, KnowledgeError> {
    Ok(Decision {
        uid: uid_column(row, 0, "decision")?,
        scope: scope_columns(row, 1)?,
        topic: row.get(3)?,
        chosen_summary: row.get(4)?,
        rationale: row.get(5)?,
        status: DecisionStatus::parse(&row.get::<_, String>(6)?)?,
        provenance: provenance_columns(row, 7)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

const APPLICATION_COLUMNS: &str = "uid, blueprint_owner_kind, blueprint_uid, scope_kind, \
    scope_key, status, application_summary, source_kind, source_locator, source_revision, \
    created_at, updated_at";

fn decode_application(row: &Row<'_>) -> Result<BlueprintApplication, KnowledgeError> {
    Ok(BlueprintApplication {
        uid: uid_column(row, 0, "blueprint_application")?,
        blueprint: BlueprintRef {
            owner: BlueprintOwnerKind::parse(&row.get::<_, String>(1)?)?,
            uid: uid_column(row, 2, "blueprint_application")?,
        },
        scope: scope_columns(row, 3)?,
        status: BlueprintApplicationStatus::parse(&row.get::<_, String>(5)?)?,
        application_summary: row.get(6)?,
        provenance: provenance_columns(row, 7)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

impl ProjectKnowledgeStore {
    /// Open the project.db at `path` directly (project-home callers and
    /// tests). Secondary worktrees use [`Self::open_project_home`].
    pub fn open(path: &Path) -> Result<Self, KnowledgeError> {
        Ok(Self::from_connection(
            schema::project::open(path)?.connection,
        ))
    }

    #[must_use]
    pub fn from_connection(connection: Connection) -> Self {
        Self { connection }
    }

    /// Open `project_id`'s canonical project.db at its registered
    /// project-home -- the one file every Workspace of the Project,
    /// including secondary worktrees, reads and writes. A missing file is
    /// reported, never created; a file bound to another Project is
    /// rejected.
    pub fn open_project_home(
        registry: &GlobalRegistry,
        project_id: ProjectId,
    ) -> Result<Self, KnowledgeError> {
        let entry = registry
            .get_project(project_id)?
            .ok_or(crate::registry::RegistryError::UnknownProject { project_id })?;
        let path = WorkspacePaths::from_root(&entry.home_locator).project_db;
        if !path.is_file() {
            return Err(KnowledgeError::ProjectHomeMissing {
                project_id,
                home_locator: entry.home_locator,
            });
        }
        let store = Self::open(&path)?;
        let bound: Option<Vec<u8>> = store.connection.query_row(
            "SELECT project_uid FROM db_meta WHERE id = 0",
            [],
            |row| row.get(0),
        )?;
        let found = bound
            .map(|bytes| uid_from_blob::<ProjectId>(&bytes, "db_meta"))
            .transpose()?;
        if found != Some(project_id) {
            return Err(KnowledgeError::ProjectIdentityMismatch {
                expected: project_id,
                found,
            });
        }
        Ok(store)
    }

    // ---- Policy ----

    /// A WORKSPACE-scoped Policy is stored here too, with the exact
    /// WorkspaceID as scope key (#20 D5) -- never copied into workspace.db.
    pub fn insert_policy(&self, new: &NewPolicy) -> Result<Policy, KnowledgeError> {
        super::insert_policy(&self.connection, &PROJECT_POLICY, new)
    }

    pub fn get_policy(&self, uid: PolicyId) -> Result<Option<Policy>, KnowledgeError> {
        super::get_policy(&self.connection, &PROJECT_POLICY, uid)
    }

    pub fn list_policies(
        &self,
        scope: &KnowledgeScope,
        status: PolicyStatus,
        limit: u32,
    ) -> Result<Vec<Policy>, KnowledgeError> {
        super::list_policies(&self.connection, &PROJECT_POLICY, scope, status, limit)
    }

    /// ACTIVE <-> DISABLED. Use [`Self::supersede_policy`] for SUPERSEDED.
    pub fn set_policy_status(
        &self,
        uid: PolicyId,
        next: PolicyStatus,
    ) -> Result<Policy, KnowledgeError> {
        super::set_policy_status(&self.connection, &PROJECT_POLICY, uid, next)
    }

    /// Atomically link `replacement SUPERSEDES superseded` and mark
    /// `superseded` SUPERSEDED. Both must exist and be ACTIVE. This is
    /// lineage only -- it is not promotion (task 4).
    pub fn supersede_policy(
        &self,
        replacement: PolicyId,
        superseded: PolicyId,
    ) -> Result<(), KnowledgeError> {
        super::supersede_policy(&self.connection, &PROJECT_POLICY, replacement, superseded)
    }

    pub fn policy_lineage(&self, uid: PolicyId) -> Result<PolicyLineage, KnowledgeError> {
        super::policy_lineage(&self.connection, &PROJECT_POLICY, uid)
    }

    // ---- Decision ----

    pub fn insert_decision(&self, new: &NewDecision) -> Result<Decision, KnowledgeError> {
        super::require_owner(&new.scope, Store::Project)?;
        let uid = DecisionId::generate();
        self.connection.execute(
            &format!(
                "INSERT INTO decision ({DECISION_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)"
            ),
            params![
                blob(uid),
                new.scope.kind().as_str(),
                new.scope.key(),
                new.topic,
                new.chosen_summary,
                new.rationale,
                DecisionStatus::Active.as_str(),
                new.provenance.source_kind.as_str(),
                new.provenance.locator,
                new.provenance.revision,
                db::now_millis_text(),
            ],
        )?;
        self.get_decision(uid)?.ok_or(KnowledgeError::NotFound {
            what: "decision",
            uid: uid.to_string(),
        })
    }

    pub fn get_decision(&self, uid: DecisionId) -> Result<Option<Decision>, KnowledgeError> {
        query_one(
            &self.connection,
            &format!("SELECT {DECISION_COLUMNS} FROM decision WHERE uid = ?1"),
            params![blob(uid)],
            decode_decision,
        )
    }

    pub fn list_decisions_by_topic(
        &self,
        topic: &str,
        status: DecisionStatus,
        limit: u32,
    ) -> Result<Vec<Decision>, KnowledgeError> {
        query_all(
            &self.connection,
            &format!(
                "SELECT {DECISION_COLUMNS} FROM decision \
                 WHERE topic = ?1 AND status = ?2 ORDER BY id LIMIT ?3"
            ),
            params![topic, status.as_str(), limit],
            decode_decision,
        )
    }

    /// Atomically link `replacement SUPERSEDES superseded` and mark
    /// `superseded` SUPERSEDED.
    pub fn supersede_decision(
        &self,
        replacement: DecisionId,
        superseded: DecisionId,
    ) -> Result<(), KnowledgeError> {
        super::link_and_retire(
            &self.connection,
            &DECISION,
            replacement,
            superseded,
            DecisionLinkKind::Supersedes.as_str(),
            Some(DecisionStatus::Superseded.as_str()),
        )
    }

    /// Atomically link `reversing REVERSES reversed` and mark `reversed`
    /// REVERSED.
    pub fn reverse_decision(
        &self,
        reversing: DecisionId,
        reversed: DecisionId,
    ) -> Result<(), KnowledgeError> {
        super::link_and_retire(
            &self.connection,
            &DECISION,
            reversing,
            reversed,
            DecisionLinkKind::Reverses.as_str(),
            Some(DecisionStatus::Reversed.as_str()),
        )
    }

    pub fn decision_lineage(&self, uid: DecisionId) -> Result<DecisionLineage, KnowledgeError> {
        let typed = |edges: Vec<(DecisionId, String)>| {
            edges
                .into_iter()
                .map(|(other, kind)| {
                    DecisionLinkKind::parse(&kind).map(|kind| DecisionLink { kind, other })
                })
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(DecisionLineage {
            outgoing: typed(super::lineage_edges(
                &self.connection,
                &DECISION,
                uid,
                true,
            )?)?,
            incoming: typed(super::lineage_edges(
                &self.connection,
                &DECISION,
                uid,
                false,
            )?)?,
        })
    }

    // ---- project-local Blueprint ----

    pub fn insert_blueprint(&self, new: &NewBlueprint) -> Result<Blueprint, KnowledgeError> {
        super::insert_blueprint(&self.connection, Store::Project, new)
    }

    pub fn get_blueprint(&self, uid: BlueprintId) -> Result<Option<Blueprint>, KnowledgeError> {
        super::get_blueprint(&self.connection, uid)
    }

    pub fn list_blueprints(
        &self,
        scope: &KnowledgeScope,
        status: BlueprintStatus,
        limit: u32,
    ) -> Result<Vec<Blueprint>, KnowledgeError> {
        super::list_blueprints(&self.connection, scope, status, limit)
    }

    pub fn set_blueprint_status(
        &self,
        uid: BlueprintId,
        next: BlueprintStatus,
    ) -> Result<Blueprint, KnowledgeError> {
        super::set_blueprint_status(&self.connection, uid, next)
    }

    // ---- Blueprint Application ----

    /// Record an application of a Blueprint. A PROJECT-owned reference must
    /// name a Blueprint in this project.db; a GLOBAL one is a stable value
    /// reference into global.db that cannot be checked from here (no
    /// cross-DB FK).
    pub fn insert_blueprint_application(
        &self,
        new: &NewBlueprintApplication,
    ) -> Result<BlueprintApplication, KnowledgeError> {
        super::require_owner(&new.scope, Store::Project)?;
        if new.blueprint.owner == BlueprintOwnerKind::Project
            && self.get_blueprint(new.blueprint.uid)?.is_none()
        {
            return Err(KnowledgeError::NotFound {
                what: "project blueprint",
                uid: new.blueprint.uid.to_string(),
            });
        }
        let uid = BlueprintApplicationId::generate();
        self.connection.execute(
            &format!(
                "INSERT INTO blueprint_application ({APPLICATION_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)"
            ),
            params![
                blob(uid),
                new.blueprint.owner.as_str(),
                blob(new.blueprint.uid),
                new.scope.kind().as_str(),
                new.scope.key(),
                BlueprintApplicationStatus::Active.as_str(),
                new.application_summary,
                new.provenance.source_kind.as_str(),
                new.provenance.locator,
                new.provenance.revision,
                db::now_millis_text(),
            ],
        )?;
        self.require_application(uid)
    }

    pub fn get_blueprint_application(
        &self,
        uid: BlueprintApplicationId,
    ) -> Result<Option<BlueprintApplication>, KnowledgeError> {
        query_one(
            &self.connection,
            &format!("SELECT {APPLICATION_COLUMNS} FROM blueprint_application WHERE uid = ?1"),
            params![blob(uid)],
            decode_application,
        )
    }

    pub fn list_blueprint_applications(
        &self,
        scope: &KnowledgeScope,
        status: BlueprintApplicationStatus,
        limit: u32,
    ) -> Result<Vec<BlueprintApplication>, KnowledgeError> {
        query_all(
            &self.connection,
            &format!(
                "SELECT {APPLICATION_COLUMNS} FROM blueprint_application \
                 WHERE scope_kind = ?1 AND scope_key IS ?2 AND status = ?3 ORDER BY id LIMIT ?4"
            ),
            params![scope.kind().as_str(), scope.key(), status.as_str(), limit],
            decode_application,
        )
    }

    /// ACTIVE -> RETIRED only; RETIRED is terminal.
    pub fn set_blueprint_application_status(
        &self,
        uid: BlueprintApplicationId,
        next: BlueprintApplicationStatus,
    ) -> Result<BlueprintApplication, KnowledgeError> {
        let current = self.require_application(uid)?;
        if current.status == BlueprintApplicationStatus::Retired || current.status == next {
            return Err(KnowledgeError::InvalidTransition {
                what: "blueprint_application",
                reason: format!("{} -> {}", current.status.as_str(), next.as_str()),
            });
        }
        self.connection.execute(
            "UPDATE blueprint_application SET status = ?1, updated_at = ?2 WHERE uid = ?3",
            params![next.as_str(), db::now_millis_text(), blob(uid)],
        )?;
        self.require_application(uid)
    }

    fn require_application(
        &self,
        uid: BlueprintApplicationId,
    ) -> Result<BlueprintApplication, KnowledgeError> {
        self.get_blueprint_application(uid)?
            .ok_or(KnowledgeError::NotFound {
                what: "blueprint_application",
                uid: uid.to_string(),
            })
    }

    // ---- Project State ----

    pub fn get_project_state(
        &self,
        key: &str,
        scope: &KnowledgeScope,
    ) -> Result<Option<ProjectState>, KnowledgeError> {
        super::get_state(&self.connection, "project_state", key, scope)
    }

    pub fn upsert_project_state(
        &self,
        update: &ProjectStateUpdate,
    ) -> Result<ProjectState, KnowledgeError> {
        super::upsert_state(&self.connection, "project_state", Store::Project, update)
    }

    pub fn list_project_state(
        &self,
        scope: &KnowledgeScope,
        limit: u32,
    ) -> Result<Vec<ProjectState>, KnowledgeError> {
        super::list_state(&self.connection, "project_state", scope, limit)
    }
}
