//! `workspace.db` I1 schema: durable, per-Workspace operational state.
//!
//! Canonical tables per #13 task 6 §4 / task 7 §16-20: `workspace_state`,
//! `work_item`, `working_state`, `work_resource`, `work_result`,
//! `work_handoff`. `db_meta`/`schema_migration` are provided by the shared
//! runner ([`crate::db`]) and are not redefined here.
//!
//! `work_resource.resource_uid` references `index.db`'s Resource by stable
//! ID value only — never a cross-DB foreign key, since a Resource row may
//! not exist yet (or may be rebuilt) independently of workspace.db's
//! lifecycle (#13 task 6 §18-19). `workspace_search_fts` is
//! optional/derived and not created in I1.
//!
//! v3 (#20 D4/D5) adds `work_note` (task-local observation / proposal /
//! open question; `promoted_item_uid` is a value reference into project.db,
//! never a FK) and `workspace_project_state` (workspace-local Project
//! State, same typed-value semantics and null-safe identity index as
//! project.db `project_state`). Policy/Decision payload is never stored
//! here.
//!
//! v4 (#20 task 3, #13 I5 task 3 amendment) adds the dirty observation
//! state `UNKNOWN | CLEAN | DIRTY` to the Working State baseline and the
//! Work Result, with a CHECK that exactly DIRTY carries a fingerprint, and
//! the `(resource_uid, role)` index behind the WorkItem overlap query.
//!
//! v5 (#20 task 3 correction) adds the index.db incarnation to the
//! baseline and result generation references; pre-v5 rows keep NULL.

use std::path::Path;

use crate::db::{self, DbKind, DbOpenError, Migration, OpenedDb};

pub const WORKSPACE_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "create_workspace_operational_tables",
        sql: "
        CREATE TABLE workspace_state (
            id INTEGER PRIMARY KEY CHECK (id = 0),
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE work_item (
            id INTEGER PRIMARY KEY,
            uid BLOB NOT NULL UNIQUE,
            source_kind TEXT NOT NULL,
            source_ref TEXT,
            title TEXT,
            goal TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at TEXT NOT NULL,
            closed_at TEXT
        );
        CREATE INDEX idx_work_item_status ON work_item (status);

        CREATE TABLE working_state (
            id INTEGER PRIMARY KEY,
            work_item_id INTEGER NOT NULL UNIQUE REFERENCES work_item (id),
            baseline_workspace_revision TEXT NOT NULL,
            baseline_generation_no INTEGER NOT NULL,
            baseline_head TEXT,
            baseline_dirty_fingerprint TEXT,
            current_step TEXT,
            progress_summary TEXT,
            remaining_summary TEXT,
            blocker_summary TEXT,
            owner_agent TEXT,
            last_observed_workspace_revision TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE work_resource (
            id INTEGER PRIMARY KEY,
            work_item_id INTEGER NOT NULL REFERENCES work_item (id),
            resource_uid BLOB NOT NULL,
            role TEXT NOT NULL,
            locator_hint TEXT,
            first_observed_revision TEXT NOT NULL,
            last_observed_revision TEXT NOT NULL,
            UNIQUE (work_item_id, resource_uid, role)
        );
        CREATE INDEX idx_work_resource_work_item ON work_resource (work_item_id);

        CREATE TABLE work_result (
            id INTEGER PRIMARY KEY,
            work_item_id INTEGER NOT NULL UNIQUE REFERENCES work_item (id),
            result_status TEXT NOT NULL,
            result_summary TEXT NOT NULL,
            commit_id TEXT,
            change_set_fingerprint TEXT,
            verification_summary TEXT,
            result_workspace_revision TEXT NOT NULL,
            result_generation_no INTEGER,
            created_at TEXT NOT NULL
        );

        CREATE TABLE work_handoff (
            id INTEGER PRIMARY KEY,
            work_item_id INTEGER NOT NULL REFERENCES work_item (id),
            handoff_summary TEXT NOT NULL,
            remaining_summary TEXT,
            blocker_summary TEXT,
            next_scope_hint TEXT,
            created_at TEXT NOT NULL
        );
        CREATE INDEX idx_work_handoff_work_item ON work_handoff (work_item_id);
    ",
    },
    Migration {
        version: 2,
        name: "add_workspace_identity_binding",
        sql: "ALTER TABLE db_meta ADD COLUMN project_uid BLOB; \
              ALTER TABLE db_meta ADD COLUMN workspace_uid BLOB;",
    },
    Migration {
        version: 3,
        name: "add_work_note_and_workspace_project_state",
        sql: "
        CREATE TABLE work_note (
            id INTEGER PRIMARY KEY,
            uid BLOB NOT NULL UNIQUE,
            work_item_id INTEGER NOT NULL REFERENCES work_item (id),
            kind TEXT NOT NULL,
            note_text TEXT NOT NULL,
            status TEXT NOT NULL,
            source_kind TEXT NOT NULL,
            source_ref TEXT,
            source_revision TEXT,
            promoted_item_kind TEXT,
            promoted_item_uid BLOB,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            CHECK ((promoted_item_kind IS NULL) = (promoted_item_uid IS NULL)),
            CHECK ((status = 'PROMOTED') = (promoted_item_uid IS NOT NULL))
        );
        CREATE INDEX idx_work_note_item_kind_status ON work_note (work_item_id, kind, status);

        CREATE TABLE workspace_project_state (
            id INTEGER PRIMARY KEY,
            uid BLOB NOT NULL UNIQUE,
            state_key TEXT NOT NULL,
            scope_kind TEXT NOT NULL,
            scope_key TEXT,
            value_type TEXT NOT NULL,
            value_json TEXT NOT NULL,
            status TEXT NOT NULL,
            source_kind TEXT NOT NULL,
            source_locator TEXT,
            observed_revision TEXT,
            updated_at TEXT NOT NULL
        );
        CREATE UNIQUE INDEX idx_workspace_project_state_identity
            ON workspace_project_state (scope_kind, ifnull(scope_key, ''), state_key);
    ",
    },
    Migration {
        version: 4,
        name: "add_dirty_observation_and_resource_lookup",
        // Legacy fingerprints are parked while the checked column is
        // added (ADD COLUMN validates its CHECK against existing rows),
        // then restored as DIRTY. A legacy NULL fingerprint stays UNKNOWN,
        // never CLEAN.
        sql: "
        CREATE TEMP TABLE legacy_baseline_dirty AS
            SELECT id, baseline_dirty_fingerprint AS fingerprint FROM working_state
            WHERE baseline_dirty_fingerprint IS NOT NULL;
        UPDATE working_state SET baseline_dirty_fingerprint = NULL
            WHERE baseline_dirty_fingerprint IS NOT NULL;
        ALTER TABLE working_state ADD COLUMN baseline_dirty_state TEXT NOT NULL DEFAULT 'UNKNOWN'
            CHECK (baseline_dirty_state IN ('UNKNOWN', 'CLEAN', 'DIRTY')
                   AND (baseline_dirty_state = 'DIRTY') = (baseline_dirty_fingerprint IS NOT NULL));
        UPDATE working_state SET baseline_dirty_state = 'DIRTY',
            baseline_dirty_fingerprint =
                (SELECT fingerprint FROM legacy_baseline_dirty l WHERE l.id = working_state.id)
            WHERE id IN (SELECT id FROM legacy_baseline_dirty);
        DROP TABLE legacy_baseline_dirty;

        ALTER TABLE work_result ADD COLUMN remaining_dirty_fingerprint TEXT;
        ALTER TABLE work_result ADD COLUMN remaining_dirty_state TEXT NOT NULL DEFAULT 'UNKNOWN'
            CHECK (remaining_dirty_state IN ('UNKNOWN', 'CLEAN', 'DIRTY')
                   AND (remaining_dirty_state = 'DIRTY') = (remaining_dirty_fingerprint IS NOT NULL));

        CREATE INDEX idx_work_resource_resource_role ON work_resource (resource_uid, role);
    ",
    },
    Migration {
        version: 5,
        name: "add_generation_reference_index_incarnation",
        // #20 task 3 correction: a generation reference names the index.db
        // incarnation it was observed in. Existing rows cannot know theirs
        // and stay NULL -- read as historical, never backfilled with the
        // current index.db's value.
        sql: "
        ALTER TABLE working_state ADD COLUMN baseline_index_incarnation_uid BLOB
            CHECK (baseline_index_incarnation_uid IS NULL
                   OR (typeof(baseline_index_incarnation_uid) = 'blob'
                       AND length(baseline_index_incarnation_uid) = 16));
        ALTER TABLE work_result ADD COLUMN result_index_incarnation_uid BLOB
            CHECK (result_index_incarnation_uid IS NULL
                   OR (typeof(result_index_incarnation_uid) = 'blob'
                       AND length(result_index_incarnation_uid) = 16
                       AND result_generation_no IS NOT NULL));
    ",
    },
];

/// Open (creating and migrating if needed) a `workspace.db` at `path`.
pub fn open(path: &Path) -> Result<OpenedDb, DbOpenError> {
    db::open(path, DbKind::Workspace, WORKSPACE_MIGRATIONS)
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
                "brainprint-schema-workspace-{label}-{}-{sequence}",
                process::id()
            ));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self(path)
        }

        fn db_path(&self) -> PathBuf {
            self.0.join("data").join("workspace.db")
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

    fn insert_work_item(opened: &OpenedDb, uid: u8, goal: &str) -> i64 {
        opened
            .connection
            .execute(
                "INSERT INTO work_item (uid, source_kind, goal, status, created_at) \
                 VALUES (?1, 'MANUAL', ?2, 'OPEN', '0')",
                params![vec![uid; 16], goal],
            )
            .expect("work_item insert should succeed");
        opened.connection.last_insert_rowid()
    }

    #[test]
    fn canonical_tables_exist() {
        let dir = TestDir::create("canonical-tables");
        let opened = open(&dir.db_path()).expect("workspace.db should migrate");

        for table in [
            "workspace_state",
            "work_item",
            "working_state",
            "work_resource",
            "work_result",
            "work_handoff",
        ] {
            assert!(
                table_exists(&opened, table),
                "missing canonical table {table}"
            );
        }
    }

    #[test]
    fn working_state_fk_requires_existing_work_item() {
        let dir = TestDir::create("working-state-fk");
        let opened = open(&dir.db_path()).expect("workspace.db should migrate");
        let work_item_id = insert_work_item(&opened, 1, "ship it");

        opened
            .connection
            .execute(
                "INSERT INTO working_state (work_item_id, baseline_workspace_revision, \
                 baseline_generation_no, last_observed_workspace_revision, updated_at) \
                 VALUES (?1, 'rev-1', 1, 'rev-1', '0')",
                params![work_item_id],
            )
            .expect("working_state for an existing work_item should succeed");

        let error = opened
            .connection
            .execute(
                "INSERT INTO working_state (work_item_id, baseline_workspace_revision, \
                 baseline_generation_no, last_observed_workspace_revision, updated_at) \
                 VALUES (?1, 'rev-1', 1, 'rev-1', '0')",
                params![999_999_i64],
            )
            .expect_err("working_state for a nonexistent work_item must be rejected");

        assert!(matches!(
            error,
            rusqlite::Error::SqliteFailure(inner, _) if inner.code == rusqlite::ErrorCode::ConstraintViolation
        ));
    }

    #[test]
    fn work_resource_preserves_stable_resource_id_without_cross_db_fk() {
        let dir = TestDir::create("work-resource");
        let opened = open(&dir.db_path()).expect("workspace.db should migrate");
        let work_item_id = insert_work_item(&opened, 2, "touch a file");
        // A ResourceID that has never been (and may never be) written to
        // index.db: work_resource must accept it with no FK to enforce.
        let resource_uid = vec![0xABu8; 16];

        opened
            .connection
            .execute(
                "INSERT INTO work_resource \
                 (work_item_id, resource_uid, role, first_observed_revision, last_observed_revision) \
                 VALUES (?1, ?2, 'TARGET', 'rev-1', 'rev-1')",
                params![work_item_id, resource_uid],
            )
            .expect("work_resource insert should succeed without any index.db present");

        let stored: Vec<u8> = opened
            .connection
            .query_row(
                "SELECT resource_uid FROM work_resource WHERE work_item_id = ?1",
                params![work_item_id],
                |row| row.get(0),
            )
            .expect("resource_uid should be queryable");
        assert_eq!(stored, resource_uid);
    }

    #[test]
    fn reopen_preserves_working_state_data() {
        let dir = TestDir::create("reopen");
        let work_item_id = {
            let opened = open(&dir.db_path()).expect("first open should migrate");
            let work_item_id = insert_work_item(&opened, 3, "durable goal");
            opened
                .connection
                .execute(
                    "INSERT INTO working_state (work_item_id, baseline_workspace_revision, \
                     baseline_generation_no, current_step, last_observed_workspace_revision, updated_at) \
                     VALUES (?1, 'rev-1', 1, 'step-1', 'rev-1', '0')",
                    params![work_item_id],
                )
                .expect("working_state insert should succeed");
            work_item_id
        };

        let reopened = open(&dir.db_path()).expect("reopen should be a no-op migration-wise");
        let step: String = reopened
            .connection
            .query_row(
                "SELECT current_step FROM working_state WHERE work_item_id = ?1",
                params![work_item_id],
                |row| row.get(0),
            )
            .expect("working_state row must survive reopen");
        assert_eq!(step, "step-1");
    }

    #[test]
    fn opening_workspace_db_as_a_different_kind_is_rejected() {
        let dir = TestDir::create("kind-mismatch");
        open(&dir.db_path()).expect("workspace.db should migrate");

        let error = db::open(
            &dir.db_path(),
            DbKind::Project,
            crate::schema::project::PROJECT_MIGRATIONS,
        )
        .expect_err("opening a workspace.db as project must be rejected");

        assert!(matches!(
            error,
            DbOpenError::KindMismatch {
                expected: DbKind::Project,
                ..
            }
        ));
    }
}
