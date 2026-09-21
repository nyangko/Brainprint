//! Common DB kind metadata and migration-runner foundation shared by the
//! four Brainprint SQLite databases (global/project/workspace/index).
//!
//! Scope (see #15 task 3 / #13 task 3, 5, 8): this module owns only the
//! mechanical open → configure → identify → migrate contract that all four
//! DB kinds share. It has no knowledge of:
//! - registry/domain schema content (#15 task 4, 5)
//! - fresh/secondary Workspace init orchestration (#15 task 6, 7)
//! - daemon connection ownership (#15 task 9)
//! - identity/DB mismatch recovery beyond a single-open kind check (#15 task 11)
//!
//! Callers supply the compiled [`Migration`] list for the DB kind they are
//! opening; that list's canonical source is Brainprint's own compiled
//! source, never a copy under a runtime `.brainprint` directory.

mod kind;
mod migration;

pub use kind::DbKind;
pub use migration::Migration;

use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

use brainprint_core::BuildInfo;
use rusqlite::{Connection, OptionalExtension, params};

/// Bounded busy timeout applied to every connection.
///
/// Placeholder; exact value is benchmark-determined (#13 task 8 §4).
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const META_BOOTSTRAP_SQL: &str = "
CREATE TABLE IF NOT EXISTS db_meta (
    id INTEGER PRIMARY KEY CHECK (id = 0),
    db_kind TEXT NOT NULL,
    schema_version INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS schema_migration (
    version INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    checksum TEXT NOT NULL,
    applied_at TEXT NOT NULL,
    app_version TEXT NOT NULL
);
";

/// A connection opened and migrated through the shared foundation.
pub struct OpenedDb {
    pub connection: Connection,
    pub kind: DbKind,
    pub schema_version: u32,
    /// Journal mode SQLite actually applied. WAL can fall back on some
    /// VFS/platforms and that fact must not be hidden (#13 task 8 §1).
    pub journal_mode: String,
}

impl fmt::Debug for OpenedDb {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenedDb")
            .field("kind", &self.kind)
            .field("schema_version", &self.schema_version)
            .field("journal_mode", &self.journal_mode)
            .finish_non_exhaustive()
    }
}

/// Failure opening, identifying, or migrating a Brainprint SQLite database.
#[derive(Debug)]
pub enum DbOpenError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Sqlite(rusqlite::Error),
    /// The compiled migration list is not strictly ordered by version.
    UnorderedMigrations {
        version: u32,
    },
    /// `db_meta.db_kind` does not match the kind the caller asked to open.
    KindMismatch {
        expected: DbKind,
        found: String,
    },
    /// An already-applied migration's checksum no longer matches the
    /// compiled migration of the same version.
    ChecksumMismatch {
        version: u32,
        name: String,
    },
    /// An already-applied migration has no matching compiled migration.
    UnknownAppliedVersion {
        version: u32,
        name: String,
    },
    /// The database's schema is newer than this binary's known migrations.
    FutureSchema {
        db_version: u32,
        max_known_version: u32,
    },
    /// A pending migration failed; its transaction was rolled back and
    /// nothing after it in the run was attempted.
    MigrationFailed {
        version: u32,
        name: String,
        source: rusqlite::Error,
    },
}

impl fmt::Display for DbOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(
                    formatter,
                    "database I/O failed at {}: {source}",
                    path.display()
                )
            }
            Self::Sqlite(source) => write!(formatter, "sqlite error: {source}"),
            Self::UnorderedMigrations { version } => write!(
                formatter,
                "compiled migration list is not strictly ordered by version at version {version}"
            ),
            Self::KindMismatch { expected, found } => write!(
                formatter,
                "expected a {expected} database, found db_meta.db_kind = \"{found}\""
            ),
            Self::ChecksumMismatch { version, name } => write!(
                formatter,
                "applied migration {version} (\"{name}\") checksum no longer matches compiled source"
            ),
            Self::UnknownAppliedVersion { version, name } => write!(
                formatter,
                "database has applied migration {version} (\"{name}\") unknown to this binary"
            ),
            Self::FutureSchema {
                db_version,
                max_known_version,
            } => write!(
                formatter,
                "database schema_version {db_version} is newer than this binary's known max version {max_known_version}"
            ),
            Self::MigrationFailed {
                version,
                name,
                source,
            } => write!(
                formatter,
                "migration {version} (\"{name}\") failed and was rolled back: {source}"
            ),
        }
    }
}

impl Error for DbOpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Sqlite(source) | Self::MigrationFailed { source, .. } => Some(source),
            Self::UnorderedMigrations { .. }
            | Self::KindMismatch { .. }
            | Self::ChecksumMismatch { .. }
            | Self::UnknownAppliedVersion { .. }
            | Self::FutureSchema { .. } => None,
        }
    }
}

impl From<rusqlite::Error> for DbOpenError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

/// Open (creating if absent) a Brainprint SQLite database of the given
/// `kind`, apply common connection configuration, verify its identity, and
/// bring it up to date with `migrations`.
///
/// `migrations` must be sorted ascending by version with no duplicates;
/// this is validated, not assumed.
pub fn open(path: &Path, kind: DbKind, migrations: &[Migration]) -> Result<OpenedDb, DbOpenError> {
    validate_migration_order(migrations)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| DbOpenError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let mut connection = Connection::open(path)?;
    configure_connection(&connection)?;
    let journal_mode = apply_journal_mode(&connection)?;
    ensure_meta_tables(&connection)?;

    let schema_version = reconcile_identity(&connection, kind)?;
    verify_applied_ledger(&connection, migrations, schema_version)?;
    let schema_version = apply_pending_migrations(&mut connection, migrations, schema_version)?;

    Ok(OpenedDb {
        connection,
        kind,
        schema_version,
        journal_mode,
    })
}

fn validate_migration_order(migrations: &[Migration]) -> Result<(), DbOpenError> {
    let mut previous: Option<u32> = None;
    for migration in migrations {
        let is_out_of_order = previous.is_some_and(|prev| migration.version <= prev);
        if is_out_of_order {
            return Err(DbOpenError::UnorderedMigrations {
                version: migration.version,
            });
        }
        previous = Some(migration.version);
    }
    Ok(())
}

fn configure_connection(connection: &Connection) -> Result<(), rusqlite::Error> {
    connection.busy_timeout(DEFAULT_BUSY_TIMEOUT)?;
    // #13 task 8 §2-3: foreign keys always enforced; NORMAL is the stated
    // default candidate pending benchmark-driven tuning.
    connection.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;")
}

fn apply_journal_mode(connection: &Connection) -> Result<String, rusqlite::Error> {
    // Report the mode SQLite actually applied rather than assuming WAL
    // succeeded (#13 task 8 §1); some VFS/platforms fall back.
    connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
}

fn ensure_meta_tables(connection: &Connection) -> Result<(), rusqlite::Error> {
    connection.execute_batch(META_BOOTSTRAP_SQL)
}

fn reconcile_identity(connection: &Connection, kind: DbKind) -> Result<u32, DbOpenError> {
    let existing: Option<(String, u32)> = connection
        .query_row(
            "SELECT db_kind, schema_version FROM db_meta WHERE id = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    match existing {
        Some((found_kind, schema_version)) => {
            if found_kind != kind.as_str() {
                return Err(DbOpenError::KindMismatch {
                    expected: kind,
                    found: found_kind,
                });
            }
            Ok(schema_version)
        }
        None => {
            connection.execute(
                "INSERT INTO db_meta (id, db_kind, schema_version) VALUES (0, ?1, 0)",
                params![kind.as_str()],
            )?;
            Ok(0)
        }
    }
}

fn verify_applied_ledger(
    connection: &Connection,
    migrations: &[Migration],
    schema_version: u32,
) -> Result<(), DbOpenError> {
    let max_known_version = migrations.last().map_or(0, |migration| migration.version);
    if schema_version > max_known_version {
        return Err(DbOpenError::FutureSchema {
            db_version: schema_version,
            max_known_version,
        });
    }

    let mut statement = connection
        .prepare("SELECT version, name, checksum FROM schema_migration ORDER BY version")?;
    let applied = statement.query_map([], |row| {
        Ok((
            row.get::<_, u32>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;

    for row in applied {
        let (version, name, checksum) = row?;
        match migrations
            .iter()
            .find(|migration| migration.version == version)
        {
            Some(migration) if migration.checksum() == checksum => {}
            Some(_) => return Err(DbOpenError::ChecksumMismatch { version, name }),
            None => return Err(DbOpenError::UnknownAppliedVersion { version, name }),
        }
    }

    Ok(())
}

fn apply_pending_migrations(
    connection: &mut Connection,
    migrations: &[Migration],
    mut schema_version: u32,
) -> Result<u32, DbOpenError> {
    let app_version = BuildInfo::current().version;
    let start_version = schema_version;

    for migration in migrations
        .iter()
        .filter(|migration| migration.version > start_version)
    {
        let tx = connection.transaction()?;

        if let Err(source) = tx.execute_batch(migration.sql) {
            return Err(DbOpenError::MigrationFailed {
                version: migration.version,
                name: migration.name.to_owned(),
                source,
            });
        }

        tx.execute(
            "INSERT INTO schema_migration (version, name, checksum, applied_at, app_version) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                migration.version,
                migration.name,
                migration.checksum(),
                now_millis_text(),
                app_version
            ],
        )?;
        tx.execute(
            "UPDATE db_meta SET schema_version = ?1 WHERE id = 0",
            params![migration.version],
        )?;

        tx.commit()?;
        schema_version = migration.version;
    }

    Ok(schema_version)
}

pub(crate) fn now_millis_text() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs, process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "brainprint-db-{label}-{}-{sequence}",
                process::id()
            ));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self(path)
        }

        /// A DB path nested under a directory that does not exist yet, to
        /// exercise bootstrap-time directory creation.
        fn db_path(&self) -> PathBuf {
            self.0.join("data").join("test.db")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const CREATE_WIDGETS: Migration = Migration {
        version: 1,
        name: "create_widgets",
        sql: "CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
    };
    const SEED_WIDGET: Migration = Migration {
        version: 2,
        name: "seed_widget",
        sql: "INSERT INTO widgets (id, name) VALUES (1, 'durable-row');",
    };
    const CREATE_GADGETS: Migration = Migration {
        version: 3,
        name: "create_gadgets",
        // References widgets(id) so applying out of order would fail.
        sql: "CREATE TABLE gadgets (\
                id INTEGER PRIMARY KEY, \
                widget_id INTEGER NOT NULL REFERENCES widgets(id)\
              );",
    };

    #[test]
    fn every_db_kind_applies_initial_migrations() {
        for kind in [
            DbKind::Global,
            DbKind::Project,
            DbKind::Workspace,
            DbKind::Index,
        ] {
            let dir = TestDir::create("initial");
            let opened = open(
                &dir.db_path(),
                kind,
                &[CREATE_WIDGETS, SEED_WIDGET, CREATE_GADGETS],
            )
            .unwrap_or_else(|error| panic!("{kind} should bootstrap: {error}"));

            assert_eq!(opened.kind, kind);
            assert_eq!(opened.schema_version, 3);
        }
    }

    #[test]
    fn reopen_does_not_reapply_migrations() {
        let dir = TestDir::create("reopen");
        let migrations = [CREATE_WIDGETS, SEED_WIDGET];

        open(&dir.db_path(), DbKind::Project, &migrations).expect("first open should migrate");
        let reopened =
            open(&dir.db_path(), DbKind::Project, &migrations).expect("reopen should be a no-op");

        assert_eq!(reopened.schema_version, 2);
        let applied_count: u32 = reopened
            .connection
            .query_row("SELECT COUNT(*) FROM schema_migration", [], |row| {
                row.get(0)
            })
            .expect("ledger should be queryable");
        assert_eq!(applied_count, 2);
        let widget_count: u32 = reopened
            .connection
            .query_row("SELECT COUNT(*) FROM widgets", [], |row| row.get(0))
            .expect("widgets should be queryable");
        assert_eq!(widget_count, 1, "seed migration must not re-run");
    }

    #[test]
    fn foreign_keys_are_enabled() {
        let dir = TestDir::create("foreign-keys");
        let opened =
            open(&dir.db_path(), DbKind::Workspace, &[]).expect("empty DB should bootstrap");

        let enabled: i64 = opened
            .connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("pragma should be queryable");
        assert_eq!(enabled, 1);
    }

    #[test]
    fn migrations_apply_in_deterministic_order() {
        let dir = TestDir::create("order");
        // If CREATE_GADGETS ran before CREATE_WIDGETS, its REFERENCES
        // clause would fail at table-creation-adjacent insert/FK check.
        let opened = open(
            &dir.db_path(),
            DbKind::Index,
            &[CREATE_WIDGETS, SEED_WIDGET, CREATE_GADGETS],
        )
        .expect("ordered migrations should all apply");

        assert_eq!(opened.schema_version, 3);
    }

    #[test]
    fn checksum_mismatch_on_applied_migration_is_rejected() {
        let dir = TestDir::create("checksum-mismatch");
        open(&dir.db_path(), DbKind::Project, &[CREATE_WIDGETS])
            .expect("first open should migrate");

        let edited = Migration {
            version: 1,
            name: "create_widgets",
            sql: "CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT NOT NULL, extra TEXT);",
        };
        let error = open(&dir.db_path(), DbKind::Project, &[edited])
            .expect_err("edited already-applied migration must be rejected");

        assert!(matches!(
            error,
            DbOpenError::ChecksumMismatch { version: 1, .. }
        ));
    }

    #[test]
    fn applied_migration_missing_from_compiled_list_is_rejected() {
        let dir = TestDir::create("unknown-applied");
        open(
            &dir.db_path(),
            DbKind::Project,
            &[CREATE_WIDGETS, SEED_WIDGET, CREATE_GADGETS],
        )
        .expect("first open should migrate all three");

        // Reopen with version 2 dropped from the compiled list while a
        // higher version (3) is still known, so this is not simply a
        // future-schema case.
        let error = open(
            &dir.db_path(),
            DbKind::Project,
            &[CREATE_WIDGETS, CREATE_GADGETS],
        )
        .expect_err("gap in compiled migrations must be rejected");

        assert!(matches!(
            error,
            DbOpenError::UnknownAppliedVersion { version: 2, .. }
        ));
    }

    #[test]
    fn failed_migration_blocks_later_migrations_and_is_not_recorded() {
        let dir = TestDir::create("mid-failure");
        let broken = Migration {
            version: 2,
            name: "broken",
            sql: "THIS IS NOT VALID SQL;",
        };
        let never_reached = Migration {
            version: 3,
            name: "never_reached",
            sql: "CREATE TABLE unreachable (id INTEGER PRIMARY KEY);",
        };

        let error = open(
            &dir.db_path(),
            DbKind::Workspace,
            &[CREATE_WIDGETS, broken, never_reached],
        )
        .expect_err("broken migration must fail the whole open");
        assert!(matches!(
            error,
            DbOpenError::MigrationFailed { version: 2, .. }
        ));

        let recovered = open(&dir.db_path(), DbKind::Workspace, &[CREATE_WIDGETS])
            .expect("valid prefix should reopen");
        assert_eq!(
            recovered.schema_version, 1,
            "only the succeeding migration is recorded"
        );
        let ledger_count: u32 = recovered
            .connection
            .query_row("SELECT COUNT(*) FROM schema_migration", [], |row| {
                row.get(0)
            })
            .expect("ledger should be queryable");
        assert_eq!(ledger_count, 1);
    }

    #[test]
    fn future_schema_is_not_silently_downgraded() {
        let dir = TestDir::create("future-schema");
        open(
            &dir.db_path(),
            DbKind::Global,
            &[CREATE_WIDGETS, SEED_WIDGET, CREATE_GADGETS],
        )
        .expect("first open should migrate to version 3");

        let error = open(&dir.db_path(), DbKind::Global, &[CREATE_WIDGETS])
            .expect_err("older binary must reject a newer on-disk schema");

        assert!(matches!(
            error,
            DbOpenError::FutureSchema {
                db_version: 3,
                max_known_version: 1
            }
        ));
    }

    #[test]
    fn wrong_db_kind_is_rejected() {
        let dir = TestDir::create("kind-mismatch");
        open(&dir.db_path(), DbKind::Project, &[]).expect("project DB should bootstrap");

        let error = open(&dir.db_path(), DbKind::Workspace, &[])
            .expect_err("opening a project DB as workspace must be rejected");

        assert!(matches!(
            error,
            DbOpenError::KindMismatch {
                expected: DbKind::Workspace,
                ..
            }
        ));
    }

    #[test]
    fn unordered_compiled_migrations_are_rejected() {
        let dir = TestDir::create("unordered");
        let out_of_order = Migration {
            version: 1,
            name: "duplicate_or_earlier",
            sql: "SELECT 1;",
        };

        let error = open(&dir.db_path(), DbKind::Index, &[SEED_WIDGET, out_of_order])
            .expect_err("non-ascending migration list must be rejected");

        assert!(matches!(
            error,
            DbOpenError::UnorderedMigrations { version: 1 }
        ));
    }

    #[test]
    fn migration_does_not_destroy_prior_durable_data() {
        let dir = TestDir::create("durable-preserved");
        open(
            &dir.db_path(),
            DbKind::Project,
            &[CREATE_WIDGETS, SEED_WIDGET],
        )
        .expect("first open should migrate");

        // A later migration adds an unrelated table; it must not touch
        // widgets' existing row.
        let reopened = open(
            &dir.db_path(),
            DbKind::Project,
            &[CREATE_WIDGETS, SEED_WIDGET, CREATE_GADGETS],
        )
        .expect("later migration should apply on top of durable data");

        let name: String = reopened
            .connection
            .query_row("SELECT name FROM widgets WHERE id = 1", [], |row| {
                row.get(0)
            })
            .expect("original durable row must survive migration");
        assert_eq!(name, "durable-row");
    }

    #[test]
    fn runner_applies_no_kind_specific_destructive_behavior() {
        // index.db is rebuildable in Brainprint's lifecycle model, but the
        // migration runner itself must not treat it differently: reopening
        // must preserve data identically across every kind.
        for kind in [DbKind::Project, DbKind::Index] {
            let dir = TestDir::create("no-destructive-branch");
            open(&dir.db_path(), kind, &[CREATE_WIDGETS, SEED_WIDGET])
                .expect("first open should migrate");
            let reopened = open(&dir.db_path(), kind, &[CREATE_WIDGETS, SEED_WIDGET])
                .expect("reopen should be a no-op");

            let widget_count: u32 = reopened
                .connection
                .query_row("SELECT COUNT(*) FROM widgets", [], |row| row.get(0))
                .expect("widgets should be queryable");
            assert_eq!(widget_count, 1, "{kind} must preserve data across reopen");
        }
    }
}
