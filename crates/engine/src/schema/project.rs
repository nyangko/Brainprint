//! `project.db` I1 schema: durable, ProjectID-owned knowledge.
//!
//! Canonical tables per #13 task 6 §3 / task 7 §12-15: `policy`,
//! `decision`, `decision_link`, `blueprint_application`, `project_state`.
//! `db_meta`/`schema_migration` are provided by the shared runner
//! ([`crate::db`]) and are not redefined here.
//!
//! `blueprint_application.blueprint_uid` references a global Blueprint by
//! stable ID value only (no cross-DB FK; global Blueprint content is never
//! duplicated into project.db). `knowledge_search_fts` is explicitly
//! optional/derived per #13 task 6 §17 and is not created in I1.

use std::path::Path;

use crate::db::{self, DbKind, DbOpenError, Migration, OpenedDb};

pub const PROJECT_MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "create_project_knowledge_tables",
    sql: "
        CREATE TABLE policy (
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
        CREATE INDEX idx_policy_scope_status ON policy (scope_kind, status);

        CREATE TABLE decision (
            id INTEGER PRIMARY KEY,
            uid BLOB NOT NULL UNIQUE,
            scope_kind TEXT NOT NULL,
            scope_key TEXT,
            topic TEXT NOT NULL,
            chosen_summary TEXT NOT NULL,
            rationale TEXT NOT NULL,
            status TEXT NOT NULL,
            source_kind TEXT NOT NULL,
            source_locator TEXT,
            source_revision TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE INDEX idx_decision_topic_status ON decision (topic, status);

        CREATE TABLE decision_link (
            decision_id INTEGER NOT NULL REFERENCES decision (id),
            related_decision_id INTEGER NOT NULL REFERENCES decision (id),
            link_kind TEXT NOT NULL,
            PRIMARY KEY (decision_id, related_decision_id, link_kind)
        );

        CREATE TABLE blueprint_application (
            id INTEGER PRIMARY KEY,
            uid BLOB NOT NULL UNIQUE,
            blueprint_uid BLOB NOT NULL,
            scope_kind TEXT NOT NULL,
            scope_key TEXT,
            status TEXT NOT NULL,
            application_summary TEXT NOT NULL,
            source_kind TEXT NOT NULL,
            source_locator TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE INDEX idx_blueprint_application_blueprint ON blueprint_application (blueprint_uid);

        CREATE TABLE project_state (
            id INTEGER PRIMARY KEY,
            state_key TEXT NOT NULL,
            scope_kind TEXT NOT NULL,
            scope_key TEXT,
            value_type TEXT NOT NULL,
            value_json TEXT NOT NULL,
            status TEXT NOT NULL,
            source_kind TEXT NOT NULL,
            source_locator TEXT,
            observed_revision TEXT,
            updated_at TEXT NOT NULL,
            UNIQUE (state_key, scope_kind, scope_key)
        );
    ",
}];

/// Open (creating and migrating if needed) a `project.db` at `path`.
pub fn open(path: &Path) -> Result<OpenedDb, DbOpenError> {
    db::open(path, DbKind::Project, PROJECT_MIGRATIONS)
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use rusqlite::{OptionalExtension, params};

    use super::*;
    use crate::db::DbOpenError;

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "brainprint-schema-project-{label}-{}-{sequence}",
                process::id()
            ));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self(path)
        }

        fn db_path(&self) -> PathBuf {
            self.0.join("data").join("project.db")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn table_exists(opened: &OpenedDb, table: &str) -> bool {
        opened
            .connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![table],
                |_| Ok(()),
            )
            .optional()
            .expect("table existence query should not fail")
            .is_some()
    }

    #[test]
    fn fresh_project_db_migrates_successfully() {
        let dir = TestDir::create("fresh");
        let opened = open(&dir.db_path()).expect("fresh project.db should migrate");
        assert_eq!(opened.schema_version, 1);
    }

    #[test]
    fn canonical_tables_exist() {
        let dir = TestDir::create("canonical-tables");
        let opened = open(&dir.db_path()).expect("project.db should migrate");

        for table in [
            "policy",
            "decision",
            "decision_link",
            "blueprint_application",
            "project_state",
        ] {
            assert!(
                table_exists(&opened, table),
                "missing canonical table {table}"
            );
        }
    }

    #[test]
    fn policy_uid_uniqueness_is_enforced() {
        let dir = TestDir::create("uid-unique");
        let opened = open(&dir.db_path()).expect("project.db should migrate");
        let uid = vec![7u8; 16];

        opened
            .connection
            .execute(
                "INSERT INTO policy (uid, scope_kind, title, rule_text, protection_class, \
                 priority_class, status, source_kind, created_at, updated_at) \
                 VALUES (?1, 'WORKSPACE', 't', 'r', 'NORMAL', 'DEFAULT', 'ACTIVE', 'MANUAL', '0', '0')",
                params![uid],
            )
            .expect("first policy insert should succeed");

        let error = opened
            .connection
            .execute(
                "INSERT INTO policy (uid, scope_kind, title, rule_text, protection_class, \
                 priority_class, status, source_kind, created_at, updated_at) \
                 VALUES (?1, 'WORKSPACE', 't2', 'r2', 'NORMAL', 'DEFAULT', 'ACTIVE', 'MANUAL', '0', '0')",
                params![uid],
            )
            .expect_err("duplicate policy uid must be rejected");

        assert!(matches!(
            error,
            rusqlite::Error::SqliteFailure(inner, _) if inner.code == rusqlite::ErrorCode::ConstraintViolation
        ));
    }

    #[test]
    fn decision_link_fk_requires_existing_decisions() {
        let dir = TestDir::create("decision-link-fk");
        let opened = open(&dir.db_path()).expect("project.db should migrate");

        let insert_decision = |uid: u8, topic: &str| {
            opened
                .connection
                .execute(
                    "INSERT INTO decision (uid, scope_kind, topic, chosen_summary, rationale, status, \
                     source_kind, created_at, updated_at) \
                     VALUES (?1, 'WORKSPACE', ?2, 'chosen', 'because', 'ACTIVE', 'MANUAL', '0', '0')",
                    params![vec![uid; 16], topic],
                )
                .expect("decision insert should succeed");
            opened.connection.last_insert_rowid()
        };

        let first = insert_decision(1, "first");
        let second = insert_decision(2, "second");

        opened
            .connection
            .execute(
                "INSERT INTO decision_link (decision_id, related_decision_id, link_kind) VALUES (?1, ?2, 'SUPERSEDES')",
                params![second, first],
            )
            .expect("linking two existing decisions should succeed");

        let error = opened
            .connection
            .execute(
                "INSERT INTO decision_link (decision_id, related_decision_id, link_kind) VALUES (?1, ?2, 'SUPERSEDES')",
                params![second, 999_999_i64],
            )
            .expect_err("linking a nonexistent decision must be rejected");

        assert!(matches!(
            error,
            rusqlite::Error::SqliteFailure(inner, _) if inner.code == rusqlite::ErrorCode::ConstraintViolation
        ));
    }

    #[test]
    fn reopen_is_idempotent_and_preserves_durable_data() {
        let dir = TestDir::create("reopen");
        let uid = vec![9u8; 16];
        {
            let opened = open(&dir.db_path()).expect("first open should migrate");
            opened
                .connection
                .execute(
                    "INSERT INTO policy (uid, scope_kind, title, rule_text, protection_class, \
                     priority_class, status, source_kind, created_at, updated_at) \
                     VALUES (?1, 'WORKSPACE', 'durable', 'r', 'NORMAL', 'DEFAULT', 'ACTIVE', 'MANUAL', '0', '0')",
                    params![uid],
                )
                .expect("policy insert should succeed");
        }

        let reopened = open(&dir.db_path()).expect("reopen should be a no-op migration-wise");
        assert_eq!(reopened.schema_version, 1);

        let ledger_count: u32 = reopened
            .connection
            .query_row("SELECT COUNT(*) FROM schema_migration", [], |row| {
                row.get(0)
            })
            .expect("ledger should be queryable");
        assert_eq!(ledger_count, 1, "migration must not reapply on reopen");

        let title: String = reopened
            .connection
            .query_row(
                "SELECT title FROM policy WHERE uid = ?1",
                params![uid],
                |row| row.get(0),
            )
            .expect("durable policy row must survive reopen");
        assert_eq!(title, "durable");
    }

    #[test]
    fn opening_project_db_as_a_different_kind_is_rejected() {
        let dir = TestDir::create("kind-mismatch");
        open(&dir.db_path()).expect("project.db should migrate");

        let error = db::open(
            &dir.db_path(),
            DbKind::Workspace,
            crate::schema::workspace::WORKSPACE_MIGRATIONS,
        )
        .expect_err("opening a project.db as workspace must be rejected");

        assert!(matches!(
            error,
            DbOpenError::KindMismatch {
                expected: DbKind::Workspace,
                ..
            }
        ));
    }
}
