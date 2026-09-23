//! global.db durable knowledge: user Policy, user Preference, reusable
//! Blueprint (#20 D1). Shares global.db's single migration history with
//! the registry ([`crate::schema::global`]).

use std::path::Path;

use brainprint_core::{BlueprintId, PolicyId, UserPreferenceId};
use rusqlite::{Connection, Row, params};

use super::{
    Blueprint, BlueprintStatus, KnowledgeError, KnowledgeScope, NewBlueprint, NewPolicy,
    NewUserPreference, Policy, PolicyLineage, PolicyStatus, PreferenceStatus, Store, TypedValue,
    USER_POLICY, UserPreference, blob, provenance_columns, query_all, query_one, scope_columns,
    uid_column,
};
use crate::{db, schema};

/// Handle to global.db's I5 durable knowledge tables.
pub struct GlobalKnowledgeStore {
    connection: Connection,
}

const PREFERENCE_COLUMNS: &str = "uid, scope_kind, scope_key, preference_key, value_type, \
    value_json, status, source_kind, source_locator, source_revision, created_at, updated_at";

fn decode_preference(row: &Row<'_>) -> Result<UserPreference, KnowledgeError> {
    Ok(UserPreference {
        uid: uid_column(row, 0, "user_preference")?,
        scope: scope_columns(row, 1)?,
        preference_key: row.get(3)?,
        value: TypedValue::from_parts(&row.get::<_, String>(4)?, &row.get::<_, String>(5)?)?,
        status: PreferenceStatus::parse(&row.get::<_, String>(6)?)?,
        provenance: provenance_columns(row, 7)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

impl GlobalKnowledgeStore {
    pub fn open(path: &Path) -> Result<Self, KnowledgeError> {
        Ok(Self::from_connection(
            schema::global::open(path)?.connection,
        ))
    }

    #[must_use]
    pub fn from_connection(connection: Connection) -> Self {
        Self { connection }
    }

    pub fn insert_user_policy(&self, new: &NewPolicy) -> Result<Policy, KnowledgeError> {
        super::insert_policy(&self.connection, &USER_POLICY, new)
    }

    pub fn get_user_policy(&self, uid: PolicyId) -> Result<Option<Policy>, KnowledgeError> {
        super::get_policy(&self.connection, &USER_POLICY, uid)
    }

    pub fn list_user_policies(
        &self,
        scope: &KnowledgeScope,
        status: PolicyStatus,
        limit: u32,
    ) -> Result<Vec<Policy>, KnowledgeError> {
        super::list_policies(&self.connection, &USER_POLICY, scope, status, limit)
    }

    /// ACTIVE <-> DISABLED. Use [`Self::supersede_user_policy`] for
    /// SUPERSEDED.
    pub fn set_user_policy_status(
        &self,
        uid: PolicyId,
        next: PolicyStatus,
    ) -> Result<Policy, KnowledgeError> {
        super::set_policy_status(&self.connection, &USER_POLICY, uid, next)
    }

    /// Atomically link `replacement SUPERSEDES superseded` and mark
    /// `superseded` SUPERSEDED. Both must exist and be ACTIVE.
    pub fn supersede_user_policy(
        &self,
        replacement: PolicyId,
        superseded: PolicyId,
    ) -> Result<(), KnowledgeError> {
        super::supersede_policy(&self.connection, &USER_POLICY, replacement, superseded)
    }

    pub fn user_policy_lineage(&self, uid: PolicyId) -> Result<PolicyLineage, KnowledgeError> {
        super::policy_lineage(&self.connection, &USER_POLICY, uid)
    }

    pub fn insert_user_preference(
        &self,
        new: &NewUserPreference,
    ) -> Result<UserPreference, KnowledgeError> {
        super::require_owner(&new.scope, Store::Global)?;
        let uid = UserPreferenceId::generate();
        self.connection.execute(
            &format!(
                "INSERT INTO user_preference ({PREFERENCE_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)"
            ),
            params![
                blob(uid),
                new.scope.kind().as_str(),
                new.scope.key(),
                new.preference_key,
                new.value.value_type().as_str(),
                new.value.to_json(),
                PreferenceStatus::Active.as_str(),
                new.provenance.source_kind.as_str(),
                new.provenance.locator,
                new.provenance.revision,
                db::now_millis_text(),
            ],
        )?;
        self.require_preference(uid)
    }

    pub fn get_user_preference(
        &self,
        uid: UserPreferenceId,
    ) -> Result<Option<UserPreference>, KnowledgeError> {
        query_one(
            &self.connection,
            &format!("SELECT {PREFERENCE_COLUMNS} FROM user_preference WHERE uid = ?1"),
            params![blob(uid)],
            decode_preference,
        )
    }

    /// Preferences for one exact scope + key with `status`.
    pub fn find_user_preferences(
        &self,
        scope: &KnowledgeScope,
        preference_key: &str,
        status: PreferenceStatus,
        limit: u32,
    ) -> Result<Vec<UserPreference>, KnowledgeError> {
        query_all(
            &self.connection,
            &format!(
                "SELECT {PREFERENCE_COLUMNS} FROM user_preference \
                 WHERE scope_kind = ?1 AND scope_key IS ?2 AND preference_key = ?3 AND status = ?4 \
                 ORDER BY id LIMIT ?5"
            ),
            params![
                scope.kind().as_str(),
                scope.key(),
                preference_key,
                status.as_str(),
                limit
            ],
            decode_preference,
        )
    }

    /// All preferences of one exact scope with `status`, by key.
    pub fn list_user_preferences(
        &self,
        scope: &KnowledgeScope,
        status: PreferenceStatus,
        limit: u32,
    ) -> Result<Vec<UserPreference>, KnowledgeError> {
        query_all(
            &self.connection,
            &format!(
                "SELECT {PREFERENCE_COLUMNS} FROM user_preference \
                 WHERE scope_kind = ?1 AND scope_key IS ?2 AND status = ?3 \
                 ORDER BY preference_key, id LIMIT ?4"
            ),
            params![scope.kind().as_str(), scope.key(), status.as_str(), limit],
            decode_preference,
        )
    }

    /// ACTIVE <-> DISABLED, and ACTIVE/DISABLED -> SUPERSEDED (terminal).
    /// Preferences have no lineage table (#20 D1).
    pub fn set_user_preference_status(
        &self,
        uid: UserPreferenceId,
        next: PreferenceStatus,
    ) -> Result<UserPreference, KnowledgeError> {
        let current = self.require_preference(uid)?;
        if current.status == PreferenceStatus::Superseded || current.status == next {
            return Err(KnowledgeError::InvalidTransition {
                what: "user_preference",
                reason: format!("{} -> {}", current.status.as_str(), next.as_str()),
            });
        }
        self.connection.execute(
            "UPDATE user_preference SET status = ?1, updated_at = ?2 WHERE uid = ?3",
            params![next.as_str(), db::now_millis_text(), blob(uid)],
        )?;
        self.require_preference(uid)
    }

    pub fn insert_blueprint(&self, new: &NewBlueprint) -> Result<Blueprint, KnowledgeError> {
        super::insert_blueprint(&self.connection, Store::Global, new)
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

    fn require_preference(&self, uid: UserPreferenceId) -> Result<UserPreference, KnowledgeError> {
        self.get_user_preference(uid)?
            .ok_or(KnowledgeError::NotFound {
                what: "user_preference",
                uid: uid.to_string(),
            })
    }
}
