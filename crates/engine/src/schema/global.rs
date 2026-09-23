//! `global.db` schema: the one ordered migration history for the whole
//! file (#20 D1).
//!
//! The registry ([`crate::registry`]) and the I5 global durable knowledge
//! store ([`crate::knowledge::GlobalKnowledgeStore`]) share this list, so
//! global.db never carries two independent version namespaces. Versions
//! 1-3 are the registry's pre-I5 migrations, unchanged byte for byte;
//! version 4 adds `user_policy`, `user_policy_link`, `user_preference`, and
//! reusable `blueprint`. There is deliberately no generic `user_knowledge`
//! table (#20 D1).

use std::path::Path;

use crate::db::{self, DbKind, DbOpenError, Migration, OpenedDb};

pub const GLOBAL_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "create_project_workspace_registry",
        sql: "
            CREATE TABLE project_registry (
                id INTEGER PRIMARY KEY,
                project_uid BLOB NOT NULL UNIQUE,
                home_locator TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX idx_project_registry_locator ON project_registry (home_locator);

            CREATE TABLE workspace_registry (
                id INTEGER PRIMARY KEY,
                workspace_uid BLOB NOT NULL UNIQUE,
                project_uid BLOB NOT NULL REFERENCES project_registry (project_uid),
                locator TEXT NOT NULL,
                is_project_home INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX idx_workspace_registry_project ON workspace_registry (project_uid);
            CREATE INDEX idx_workspace_registry_locator ON workspace_registry (locator);
        ",
    },
    Migration {
        version: 2,
        name: "create_project_git_lineage",
        sql: "
            CREATE TABLE project_git_lineage (
                id INTEGER PRIMARY KEY,
                git_common_dir TEXT NOT NULL UNIQUE,
                project_uid BLOB NOT NULL REFERENCES project_registry (project_uid),
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX idx_project_git_lineage_project ON project_git_lineage (project_uid);
        ",
    },
    Migration {
        version: 3,
        name: "add_registry_active_missing_last_seen",
        sql: "
            ALTER TABLE project_registry ADD COLUMN state TEXT NOT NULL DEFAULT 'ACTIVE';
            ALTER TABLE project_registry ADD COLUMN last_seen_at TEXT;
            ALTER TABLE workspace_registry ADD COLUMN state TEXT NOT NULL DEFAULT 'ACTIVE';
            ALTER TABLE workspace_registry ADD COLUMN last_seen_at TEXT;
        ",
    },
    Migration {
        version: 4,
        name: "create_global_durable_knowledge",
        sql: "
            CREATE TABLE user_policy (
                id INTEGER PRIMARY KEY,
                uid BLOB NOT NULL UNIQUE,
                scope_kind TEXT NOT NULL,
                scope_key TEXT,
                policy_key TEXT,
                title TEXT NOT NULL,
                rule_text TEXT NOT NULL,
                structured_rule_json TEXT,
                protection_class TEXT NOT NULL,
                priority_class TEXT NOT NULL,
                status TEXT NOT NULL,
                source_kind TEXT NOT NULL,
                source_locator TEXT,
                source_revision TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX idx_user_policy_scope_status ON user_policy (scope_kind, scope_key, status);

            CREATE TABLE user_policy_link (
                user_policy_id INTEGER NOT NULL REFERENCES user_policy (id),
                related_user_policy_id INTEGER NOT NULL REFERENCES user_policy (id),
                link_kind TEXT NOT NULL,
                PRIMARY KEY (user_policy_id, related_user_policy_id, link_kind)
            );
            CREATE INDEX idx_user_policy_link_related ON user_policy_link (related_user_policy_id);

            CREATE TABLE user_preference (
                id INTEGER PRIMARY KEY,
                uid BLOB NOT NULL UNIQUE,
                scope_kind TEXT NOT NULL,
                scope_key TEXT,
                preference_key TEXT NOT NULL,
                value_type TEXT NOT NULL,
                value_json TEXT NOT NULL,
                status TEXT NOT NULL,
                source_kind TEXT NOT NULL,
                source_locator TEXT,
                source_revision TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX idx_user_preference_scope_key_status
                ON user_preference (scope_kind, scope_key, preference_key, status);

            CREATE TABLE blueprint (
                id INTEGER PRIMARY KEY,
                uid BLOB NOT NULL UNIQUE,
                scope_kind TEXT NOT NULL,
                scope_key TEXT,
                blueprint_key TEXT,
                title TEXT NOT NULL,
                intent TEXT NOT NULL,
                definition_json TEXT NOT NULL,
                status TEXT NOT NULL,
                version TEXT,
                source_kind TEXT NOT NULL,
                source_locator TEXT,
                source_revision TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX idx_blueprint_scope_status ON blueprint (scope_kind, scope_key, status);
        ",
    },
];

/// Open (creating and migrating if needed) a `global.db` at `path`.
pub fn open(path: &Path) -> Result<OpenedDb, DbOpenError> {
    db::open(path, DbKind::Global, GLOBAL_MIGRATIONS)
}
