//! I1 schema / storage-contract foundation for `project.db`, `workspace.db`,
//! and `index.db` (see #15 task 5, #13 task 6-7).
//!
//! Each submodule owns only that DB kind's compiled [`Migration`](crate::db::Migration)
//! list plus a thin `open()` that pairs it with the right [`DbKind`](crate::db::DbKind)
//! through task 3's shared runner. There is deliberately no typed
//! CRUD/domain API here: this task creates table structure only — no
//! Resource discovery, Tree-sitter, relation extraction, semantic
//! resolution, or generation-publication logic. Those remain later #15
//! tasks (6-11); building typed accessors for empty tables now would be
//! scope creep ahead of the logic that would actually use them.

pub mod global;
pub mod index;
pub mod project;
pub mod workspace;

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use rusqlite::Connection;

    use super::*;

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "brainprint-schema-mod-{label}-{}-{sequence}",
                process::id()
            ));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self(path)
        }

        fn db_path(&self, file: &str) -> PathBuf {
            self.0.join("data").join(file)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn table_names(connection: &Connection) -> Vec<String> {
        let mut statement = connection
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .expect("sqlite_master should be queryable");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("table listing should succeed")
            .collect::<Result<Vec<_>, _>>()
            .expect("table names should decode")
    }

    fn column_names(connection: &Connection, table: &str) -> Vec<String> {
        let mut statement = connection
            .prepare(&format!("PRAGMA table_info({table})"))
            .expect("table_info should be queryable");
        statement
            .query_map([], |row| row.get::<_, String>(1))
            .expect("column listing should succeed")
            .collect::<Result<Vec<_>, _>>()
            .expect("column names should decode")
    }

    #[test]
    fn no_schema_stores_raw_source_body() {
        let dir = TestDir::create("no-raw-body");
        let databases = [
            project::open(&dir.db_path("project.db")).expect("project.db should migrate"),
            workspace::open(&dir.db_path("workspace.db")).expect("workspace.db should migrate"),
            index::open(&dir.db_path("index.db")).expect("index.db should migrate"),
        ];

        const DISALLOWED_COLUMN_NAMES: &[&str] = &[
            "content",
            "source_body",
            "raw_content",
            "file_content",
            "body_text",
            "source_text",
        ];

        for opened in &databases {
            for table in table_names(&opened.connection) {
                if table == "schema_migration" || table == "db_meta" {
                    continue;
                }
                for column in column_names(&opened.connection, &table) {
                    let normalized = column.to_lowercase();
                    assert!(
                        !DISALLOWED_COLUMN_NAMES.contains(&normalized.as_str()),
                        "{table}.{column} looks like a raw source body column"
                    );
                }
            }
        }
    }

    #[test]
    fn db_kinds_do_not_mix_table_ownership() {
        let dir = TestDir::create("no-ownership-mixing");
        let project_db =
            project::open(&dir.db_path("project.db")).expect("project.db should migrate");
        let workspace_db =
            workspace::open(&dir.db_path("workspace.db")).expect("workspace.db should migrate");
        let index_db = index::open(&dir.db_path("index.db")).expect("index.db should migrate");

        let project_tables = table_names(&project_db.connection);
        let workspace_tables = table_names(&workspace_db.connection);
        let index_tables = table_names(&index_db.connection);

        for table in [
            "work_item",
            "working_state",
            "resource",
            "symbol",
            "relation",
        ] {
            assert!(
                !project_tables.contains(&table.to_string()),
                "project.db must not own {table}"
            );
        }
        for table in ["policy", "decision", "resource", "symbol", "relation"] {
            assert!(
                !workspace_tables.contains(&table.to_string()),
                "workspace.db must not own {table}"
            );
        }
        for table in ["policy", "decision", "work_item", "working_state"] {
            assert!(
                !index_tables.contains(&table.to_string()),
                "index.db must not own {table}"
            );
        }
    }
}
