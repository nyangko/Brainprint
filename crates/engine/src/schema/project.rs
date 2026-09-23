//! `project.db` I1 schema: durable, ProjectID-owned knowledge.
//!
//! Canonical tables per #13 task 6 §3 / task 7 §12-15: `policy`,
//! `decision`, `decision_link`, `blueprint_application`, `project_state`.
//! `db_meta`/`schema_migration` are provided by the shared runner
//! ([`crate::db`]) and are not redefined here.
//!
//! `blueprint_application.blueprint_uid` references a Blueprint by stable
//! ID value only; `blueprint_owner_kind` (v3, #20 D2) says which store owns
//! the definition -- GLOBAL (global.db, never duplicated here, no cross-DB
//! FK) or PROJECT (this file's own `blueprint`). Pre-v3 rows were written
//! under the global-reference-only contract and migrate as GLOBAL.
//!
//! v3 also adds `policy_link` (#20 D3) and fixes `project_state`'s
//! canonical identity: `UNIQUE (state_key, scope_kind, scope_key)` treats
//! two NULL `scope_key`s as distinct, so a unique expression index over
//! `ifnull(scope_key, '')` enforces one row per key and scope. Typed
//! scopes never carry an empty key, so `''` cannot collide with a real
//! one. `project_state.uid` gives each entry the stable identity a
//! `work_note` promotion target references across DBs (#20 D4).
//!
//! v4 (#20 task 4) adds `knowledge_promotion`, the one-row-per-promoted-
//! WorkNote receipt that makes the workspace.db → project.db promotion
//! retryable without a cross-DB transaction.
//!
//! `knowledge_search_fts` is explicitly optional/derived per #13 task 6 §17
//! and is not created.

use std::path::Path;

use crate::db::{self, DbKind, DbOpenError, Migration, OpenedDb};

pub const PROJECT_MIGRATIONS: &[Migration] = &[
    Migration {
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
    },
    Migration {
        version: 2,
        name: "add_project_identity_binding",
        sql: "ALTER TABLE db_meta ADD COLUMN project_uid BLOB;",
    },
    Migration {
        version: 3,
        name: "add_project_intelligence_lineage_blueprint_state_identity",
        sql: "
        CREATE TABLE policy_link (
            policy_id INTEGER NOT NULL REFERENCES policy (id),
            related_policy_id INTEGER NOT NULL REFERENCES policy (id),
            link_kind TEXT NOT NULL,
            PRIMARY KEY (policy_id, related_policy_id, link_kind)
        );
        CREATE INDEX idx_policy_link_related ON policy_link (related_policy_id);
        CREATE INDEX idx_decision_link_related ON decision_link (related_decision_id);

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

        ALTER TABLE blueprint_application
            ADD COLUMN blueprint_owner_kind TEXT NOT NULL DEFAULT 'GLOBAL';
        ALTER TABLE blueprint_application ADD COLUMN source_revision TEXT;
        CREATE INDEX idx_blueprint_application_scope_status
            ON blueprint_application (scope_kind, scope_key, status);

        ALTER TABLE project_state ADD COLUMN uid BLOB;
        UPDATE project_state SET uid = randomblob(16) WHERE uid IS NULL;
        CREATE UNIQUE INDEX idx_project_state_uid ON project_state (uid);
        CREATE UNIQUE INDEX idx_project_state_identity
            ON project_state (scope_kind, ifnull(scope_key, ''), state_key);
    ",
    },
    Migration {
        version: 4,
        name: "add_knowledge_promotion_receipt",
        // #20 task 4 / #13 I5 task 4 amendment. One row per promoted
        // WorkNote: the crash-safe idempotency key of the two-DB promotion
        // protocol and its provenance link. `target_uid` names a policy,
        // decision, or project_state row by stable ID; there is no generic
        // FK across three tables. The CHECKs restate the P0 category and
        // authority gates so a hand-written row cannot contradict them.
        sql: "
        CREATE TABLE knowledge_promotion (
            id INTEGER PRIMARY KEY,
            uid BLOB NOT NULL UNIQUE CHECK (typeof(uid) = 'blob' AND length(uid) = 16),
            workspace_uid BLOB NOT NULL
                CHECK (typeof(workspace_uid) = 'blob' AND length(workspace_uid) = 16),
            work_note_uid BLOB NOT NULL
                CHECK (typeof(work_note_uid) = 'blob' AND length(work_note_uid) = 16),
            work_note_kind TEXT NOT NULL,
            work_note_source_kind TEXT NOT NULL,
            work_note_fingerprint TEXT NOT NULL,
            target_kind TEXT NOT NULL,
            target_uid BLOB NOT NULL
                CHECK (typeof(target_uid) = 'blob' AND length(target_uid) = 16),
            request_fingerprint TEXT NOT NULL,
            promotion_basis TEXT NOT NULL,
            authority_source_kind TEXT NOT NULL,
            authority_source_locator TEXT,
            authority_source_revision TEXT,
            lineage_kind TEXT,
            lineage_target_uid BLOB
                CHECK (lineage_target_uid IS NULL
                       OR (typeof(lineage_target_uid) = 'blob' AND length(lineage_target_uid) = 16)),
            created_at TEXT NOT NULL,
            UNIQUE (workspace_uid, work_note_uid),
            CHECK ((work_note_kind = 'PROPOSAL' AND target_kind IN ('POLICY', 'DECISION'))
                   OR (work_note_kind = 'OBSERVATION' AND target_kind = 'PROJECT_STATE')),
            CHECK ((promotion_basis = 'USER_EXPLICIT'
                        AND authority_source_kind = 'USER_EXPLICIT')
                   OR (promotion_basis = 'AUTHORITATIVE_ARTIFACT'
                        AND authority_source_kind = 'AUTHORITATIVE_ARTIFACT')
                   OR (promotion_basis = 'VALIDATED_PROJECT_OBSERVATION'
                        AND authority_source_kind = 'OBSERVED'
                        AND target_kind = 'PROJECT_STATE')),
            CHECK ((lineage_kind IS NULL) = (lineage_target_uid IS NULL)),
            CHECK (lineage_kind IS NULL
                   OR (lineage_kind = 'POLICY_SUPERSEDES' AND target_kind = 'POLICY')
                   OR (lineage_kind IN ('DECISION_SUPERSEDES', 'DECISION_REVERSES')
                       AND target_kind = 'DECISION'))
        );
    ",
    },
];

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
        assert_eq!(opened.schema_version, 4);
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
        assert_eq!(reopened.schema_version, 4);

        let ledger_count: u32 = reopened
            .connection
            .query_row("SELECT COUNT(*) FROM schema_migration", [], |row| {
                row.get(0)
            })
            .expect("ledger should be queryable");
        assert_eq!(ledger_count, 4, "migration must not reapply on reopen");

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
