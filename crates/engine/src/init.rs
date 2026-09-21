//! Fresh Workspace init orchestration: Git and non-Git (#15 task 6 / #13
//! task 3 §7, task 5 §4, task 5 §17).
//!
//! This connects tasks 1-5 (identity types, path/config bootstrap, the
//! migration runner, the global registry, and project/workspace/index
//! schema) into one orchestration for the *fresh* case only: a directory
//! Brainprint has never touched before, Git or not. A secondary Git
//! worktree of an already-registered Project is explicitly out of scope
//! (#15 task 7) — this module makes no attempt to detect or link one; a
//! worktree directory with its own `.git` is treated as an independent
//! fresh case, and that is by design, not an oversight.
//!
//! The whole flow is written to be safely re-callable: if `<workspace>/.brainprint/workspace.toml`
//! already holds a valid identity, that identity is reused end to end
//! (never overwritten with a freshly generated one — #15 task 6 rule 14),
//! and every downstream step (config bootstrap, DB open/migrate, identity
//! binding, registry registration) is naturally idempotent for a matching
//! identity. That gives a caller a `Created` vs `AlreadyInitialized`
//! answer, and makes a resumed call after a partial failure complete the
//! same identity rather than risk minting a second one.

use std::{
    error::Error,
    fmt,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use brainprint_core::{ProjectId, WorkspaceId};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::{
    config::{self, ConfigError},
    db::{self, DbKind, DbOpenError},
    paths::{GlobalPaths, WorkspacePaths},
    registry::{GlobalRegistry, RegistryError},
    schema,
};

const IDENTITY_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceIdentity {
    format_version: u32,
    project_id: ProjectId,
    workspace_id: WorkspaceId,
    created_at: String,
}

/// Result of a fresh-init call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitOutcome {
    pub project_id: ProjectId,
    pub workspace_id: WorkspaceId,
    pub workspace_root: PathBuf,
    pub is_git: bool,
    /// `true` if this call generated a new Project/Workspace identity;
    /// `false` if it resumed/verified an already-initialized Workspace.
    pub freshly_created: bool,
}

/// Failure during fresh Workspace init.
#[derive(Debug)]
pub enum InitError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    MissingParent {
        path: PathBuf,
    },
    MalformedIdentity {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    UnsupportedIdentityFormat {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    EncodeIdentity {
        source: Box<toml::ser::Error>,
    },
    Config(ConfigError),
    Db(DbOpenError),
    Registry(RegistryError),
    Sqlite(rusqlite::Error),
    /// `project.db`'s bound `project_uid` does not match this Workspace's
    /// `workspace.toml` identity.
    ProjectIdentityMismatch {
        expected: ProjectId,
        found: ProjectId,
    },
    /// `workspace.db`/`index.db`'s bound identity does not match this
    /// Workspace's `workspace.toml` identity.
    WorkspaceIdentityMismatch {
        expected_project: ProjectId,
        expected_workspace: WorkspaceId,
        found_project: ProjectId,
        found_workspace: WorkspaceId,
    },
    /// `db_meta` has only one of `project_uid`/`workspace_uid` set, which
    /// this module never writes and therefore cannot interpret safely.
    CorruptDbMeta {
        kind: DbKind,
    },
}

impl fmt::Display for InitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(formatter, "init I/O failed at {}: {source}", path.display())
            }
            Self::MissingParent { path } => {
                write!(formatter, "identity path has no parent: {}", path.display())
            }
            Self::MalformedIdentity { path, source } => {
                write!(
                    formatter,
                    "invalid workspace identity at {}: {source}",
                    path.display()
                )
            }
            Self::UnsupportedIdentityFormat {
                path,
                found,
                supported,
            } => write!(
                formatter,
                "unsupported workspace identity format at {}: found {found}, supported {supported}",
                path.display()
            ),
            Self::EncodeIdentity { source } => {
                write!(formatter, "failed to encode workspace identity: {source}")
            }
            Self::Config(source) => write!(formatter, "{source}"),
            Self::Db(source) => write!(formatter, "{source}"),
            Self::Registry(source) => write!(formatter, "{source}"),
            Self::Sqlite(source) => write!(formatter, "init sqlite error: {source}"),
            Self::ProjectIdentityMismatch { expected, found } => write!(
                formatter,
                "project.db is bound to project {found}, but this workspace's identity is {expected}"
            ),
            Self::WorkspaceIdentityMismatch {
                expected_project,
                expected_workspace,
                found_project,
                found_workspace,
            } => write!(
                formatter,
                "database is bound to project {found_project} / workspace {found_workspace}, \
                 but this workspace's identity is project {expected_project} / workspace {expected_workspace}"
            ),
            Self::CorruptDbMeta { kind } => {
                write!(
                    formatter,
                    "{kind} db_meta has a partially-set identity binding"
                )
            }
        }
    }
}

impl Error for InitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::MalformedIdentity { source, .. } => Some(source.as_ref()),
            Self::EncodeIdentity { source } => Some(source.as_ref()),
            Self::Config(source) => Some(source),
            Self::Db(source) => Some(source),
            Self::Registry(source) => Some(source),
            Self::Sqlite(source) => Some(source),
            Self::MissingParent { .. }
            | Self::UnsupportedIdentityFormat { .. }
            | Self::ProjectIdentityMismatch { .. }
            | Self::WorkspaceIdentityMismatch { .. }
            | Self::CorruptDbMeta { .. } => None,
        }
    }
}

impl From<ConfigError> for InitError {
    fn from(source: ConfigError) -> Self {
        Self::Config(source)
    }
}

impl From<DbOpenError> for InitError {
    fn from(source: DbOpenError) -> Self {
        Self::Db(source)
    }
}

impl From<RegistryError> for InitError {
    fn from(source: RegistryError) -> Self {
        Self::Registry(source)
    }
}

impl From<rusqlite::Error> for InitError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

/// Init `requested_root` as a fresh Brainprint Workspace, or verify/resume
/// one already initialized there.
///
/// For a Git repository or worktree, the Workspace root is the nearest
/// ancestor containing `.git` (not necessarily `requested_root` itself);
/// for a non-Git directory, `requested_root` itself is the Workspace root.
/// `global_paths` is the already-resolved `~/.brainprint` location (see
/// [`GlobalPaths::discover`] / [`GlobalPaths::from_home`]).
pub fn init_fresh_workspace(
    requested_root: &Path,
    global_paths: &GlobalPaths,
) -> Result<InitOutcome, InitError> {
    let canonical_start = fs::canonicalize(requested_root).map_err(|source| InitError::Io {
        path: requested_root.to_path_buf(),
        source,
    })?;

    let git_root = find_git_root(&canonical_start);
    let is_git = git_root.is_some();
    let workspace_root = git_root.unwrap_or(canonical_start);

    let workspace_paths = WorkspacePaths::from_root(&workspace_root);

    let (identity, freshly_created) = match read_identity_file(&workspace_paths.identity_file)? {
        Some(existing) => (existing, false),
        None => {
            let identity = WorkspaceIdentity {
                format_version: IDENTITY_FORMAT_VERSION,
                project_id: ProjectId::generate(),
                workspace_id: WorkspaceId::generate(),
                created_at: db::now_millis_text(),
            };
            write_identity_file(&workspace_paths.identity_file, &identity)?;
            (identity, true)
        }
    };

    config::bootstrap_workspace_config(&workspace_paths)?;

    let project_db = schema::project::open(&workspace_paths.project_db)?;
    bind_or_verify_project_identity(&project_db.connection, identity.project_id)?;

    let workspace_db = schema::workspace::open(&workspace_paths.workspace_db)?;
    bind_or_verify_workspace_scoped_identity(
        &workspace_db.connection,
        DbKind::Workspace,
        identity.project_id,
        identity.workspace_id,
    )?;

    let index_db = schema::index::open(&workspace_paths.index_db)?;
    bind_or_verify_workspace_scoped_identity(
        &index_db.connection,
        DbKind::Index,
        identity.project_id,
        identity.workspace_id,
    )?;

    let registry = GlobalRegistry::open(&global_paths.global_db)?;
    registry.register_project(identity.project_id, &workspace_root)?;
    registry.register_workspace(
        identity.workspace_id,
        identity.project_id,
        &workspace_root,
        true,
    )?;

    Ok(InitOutcome {
        project_id: identity.project_id,
        workspace_id: identity.workspace_id,
        workspace_root,
        is_git,
        freshly_created,
    })
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn read_identity_file(path: &Path) -> Result<Option<WorkspaceIdentity>, InitError> {
    if !path.exists() {
        return Ok(None);
    }

    let text = fs::read_to_string(path).map_err(|source| InitError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let identity: WorkspaceIdentity =
        toml::from_str(&text).map_err(|source| InitError::MalformedIdentity {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;

    if identity.format_version != IDENTITY_FORMAT_VERSION {
        return Err(InitError::UnsupportedIdentityFormat {
            path: path.to_path_buf(),
            found: identity.format_version,
            supported: IDENTITY_FORMAT_VERSION,
        });
    }

    Ok(Some(identity))
}

fn write_identity_file(path: &Path, identity: &WorkspaceIdentity) -> Result<(), InitError> {
    let mut encoded =
        toml::to_string_pretty(identity).map_err(|source| InitError::EncodeIdentity {
            source: Box::new(source),
        })?;
    if !encoded.ends_with('\n') {
        encoded.push('\n');
    }

    let parent = path.parent().ok_or_else(|| InitError::MissingParent {
        path: path.to_path_buf(),
    })?;
    fs::create_dir_all(parent).map_err(|source| InitError::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => file
            .write_all(encoded.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|source| InitError::Io {
                path: path.to_path_buf(),
                source,
            }),
        // A concurrent writer already created it; #15 task 6 rule 14 says
        // never overwrite an existing identity with a freshly generated
        // one, so this call simply proceeds without writing.
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(source) => Err(InitError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn bind_or_verify_project_identity(
    connection: &Connection,
    project_id: ProjectId,
) -> Result<(), InitError> {
    let existing: Option<Vec<u8>> =
        connection.query_row("SELECT project_uid FROM db_meta WHERE id = 0", [], |row| {
            row.get(0)
        })?;

    match existing {
        None => {
            connection.execute(
                "UPDATE db_meta SET project_uid = ?1 WHERE id = 0",
                rusqlite::params![project_id.to_bytes().to_vec()],
            )?;
            Ok(())
        }
        Some(bytes) => {
            let found = stable_id_from_blob::<ProjectId>(&bytes, DbKind::Project)?;
            if found == project_id {
                Ok(())
            } else {
                Err(InitError::ProjectIdentityMismatch {
                    expected: project_id,
                    found,
                })
            }
        }
    }
}

fn bind_or_verify_workspace_scoped_identity(
    connection: &Connection,
    kind: DbKind,
    project_id: ProjectId,
    workspace_id: WorkspaceId,
) -> Result<(), InitError> {
    let existing: (Option<Vec<u8>>, Option<Vec<u8>>) = connection.query_row(
        "SELECT project_uid, workspace_uid FROM db_meta WHERE id = 0",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    match existing {
        (None, None) => {
            connection.execute(
                "UPDATE db_meta SET project_uid = ?1, workspace_uid = ?2 WHERE id = 0",
                rusqlite::params![
                    project_id.to_bytes().to_vec(),
                    workspace_id.to_bytes().to_vec()
                ],
            )?;
            Ok(())
        }
        (Some(project_bytes), Some(workspace_bytes)) => {
            let found_project = stable_id_from_blob::<ProjectId>(&project_bytes, kind)?;
            let found_workspace = stable_id_from_blob::<WorkspaceId>(&workspace_bytes, kind)?;
            if found_project == project_id && found_workspace == workspace_id {
                Ok(())
            } else {
                Err(InitError::WorkspaceIdentityMismatch {
                    expected_project: project_id,
                    expected_workspace: workspace_id,
                    found_project,
                    found_workspace,
                })
            }
        }
        (None, Some(_)) | (Some(_), None) => Err(InitError::CorruptDbMeta { kind }),
    }
}

trait StableIdFromBytes: Sized {
    fn from_bytes_16(bytes: [u8; 16]) -> Self;
}

impl StableIdFromBytes for ProjectId {
    fn from_bytes_16(bytes: [u8; 16]) -> Self {
        Self::from_bytes(bytes)
    }
}

impl StableIdFromBytes for WorkspaceId {
    fn from_bytes_16(bytes: [u8; 16]) -> Self {
        Self::from_bytes(bytes)
    }
}

fn stable_id_from_blob<T: StableIdFromBytes>(bytes: &[u8], kind: DbKind) -> Result<T, InitError> {
    let array: [u8; 16] = bytes
        .try_into()
        .map_err(|_| InitError::CorruptDbMeta { kind })?;
    Ok(T::from_bytes_16(array))
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs, process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use rusqlite::params;

    use super::*;

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "brainprint-init-{label}-{}-{sequence}",
                process::id()
            ));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn global_paths(home: &TestDir) -> GlobalPaths {
        GlobalPaths::from_home(home.path())
    }

    fn make_git_repo(workspace: &TestDir) {
        fs::create_dir_all(workspace.path().join(".git"))
            .expect(".git directory should be created");
    }

    #[test]
    fn fresh_git_repository_init_succeeds() {
        let global_home = TestDir::create("git-fresh-global");
        let workspace = TestDir::create("git-fresh-workspace");
        make_git_repo(&workspace);

        let outcome = init_fresh_workspace(workspace.path(), &global_paths(&global_home))
            .expect("fresh git init should succeed");

        assert!(outcome.is_git);
        assert!(outcome.freshly_created);
        assert_eq!(
            outcome.workspace_root,
            fs::canonicalize(workspace.path()).expect("workspace path should canonicalize")
        );
    }

    #[test]
    fn project_home_equals_current_workspace_in_registry() {
        let global_home = TestDir::create("git-project-home-global");
        let workspace = TestDir::create("git-project-home-workspace");
        make_git_repo(&workspace);
        let paths = global_paths(&global_home);

        let outcome = init_fresh_workspace(workspace.path(), &paths).expect("init should succeed");

        let registry = GlobalRegistry::open(&paths.global_db).expect("registry should open");
        let project = registry
            .get_project(outcome.project_id)
            .expect("lookup should succeed")
            .expect("project should be registered");
        assert_eq!(project.home_locator, outcome.workspace_root);

        let workspace_entry = registry
            .get_workspace(outcome.workspace_id)
            .expect("lookup should succeed")
            .expect("workspace should be registered");
        assert!(workspace_entry.is_project_home);
        assert_eq!(workspace_entry.locator, outcome.workspace_root);
        assert_eq!(workspace_entry.project_id, outcome.project_id);
    }

    #[test]
    fn workspace_toml_is_created_with_matching_identity() {
        let global_home = TestDir::create("workspace-toml-global");
        let workspace = TestDir::create("workspace-toml-workspace");
        make_git_repo(&workspace);
        let paths = global_paths(&global_home);

        let outcome = init_fresh_workspace(workspace.path(), &paths).expect("init should succeed");

        let workspace_paths = WorkspacePaths::from_root(&outcome.workspace_root);
        assert!(workspace_paths.identity_file.is_file());
        let identity = read_identity_file(&workspace_paths.identity_file)
            .expect("identity should be readable")
            .expect("identity should be present");
        assert_eq!(identity.project_id, outcome.project_id);
        assert_eq!(identity.workspace_id, outcome.workspace_id);
    }

    #[test]
    fn project_workspace_index_dbs_are_created_at_project_home() {
        let global_home = TestDir::create("dbs-global");
        let workspace = TestDir::create("dbs-workspace");
        make_git_repo(&workspace);
        let paths = global_paths(&global_home);

        let outcome = init_fresh_workspace(workspace.path(), &paths).expect("init should succeed");
        let workspace_paths = WorkspacePaths::from_root(&outcome.workspace_root);

        assert!(workspace_paths.project_db.is_file());
        assert!(workspace_paths.workspace_db.is_file());
        assert!(workspace_paths.index_db.is_file());
    }

    #[test]
    fn db_identity_matches_workspace_toml_identity() {
        let global_home = TestDir::create("db-identity-global");
        let workspace = TestDir::create("db-identity-workspace");
        make_git_repo(&workspace);
        let paths = global_paths(&global_home);

        let outcome = init_fresh_workspace(workspace.path(), &paths).expect("init should succeed");
        let workspace_paths = WorkspacePaths::from_root(&outcome.workspace_root);

        let project_db =
            schema::project::open(&workspace_paths.project_db).expect("project.db should reopen");
        let bound_project: Vec<u8> = project_db
            .connection
            .query_row("SELECT project_uid FROM db_meta WHERE id = 0", [], |row| {
                row.get(0)
            })
            .expect("project_uid should be set");
        assert_eq!(bound_project, outcome.project_id.to_bytes().to_vec());

        let workspace_db = schema::workspace::open(&workspace_paths.workspace_db)
            .expect("workspace.db should reopen");
        let (bound_p, bound_w): (Vec<u8>, Vec<u8>) = workspace_db
            .connection
            .query_row(
                "SELECT project_uid, workspace_uid FROM db_meta WHERE id = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("identity should be set");
        assert_eq!(bound_p, outcome.project_id.to_bytes().to_vec());
        assert_eq!(bound_w, outcome.workspace_id.to_bytes().to_vec());

        let index_db =
            schema::index::open(&workspace_paths.index_db).expect("index.db should reopen");
        let (bound_p2, bound_w2): (Vec<u8>, Vec<u8>) = index_db
            .connection
            .query_row(
                "SELECT project_uid, workspace_uid FROM db_meta WHERE id = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("identity should be set");
        assert_eq!(bound_p2, outcome.project_id.to_bytes().to_vec());
        assert_eq!(bound_w2, outcome.workspace_id.to_bytes().to_vec());
    }

    #[test]
    fn fresh_non_git_directory_init_succeeds() {
        let global_home = TestDir::create("non-git-global");
        let workspace = TestDir::create("non-git-workspace");
        let paths = global_paths(&global_home);

        let outcome =
            init_fresh_workspace(workspace.path(), &paths).expect("non-git init should succeed");

        assert!(!outcome.is_git);
        assert!(outcome.freshly_created);

        let workspace_paths = WorkspacePaths::from_root(&outcome.workspace_root);
        assert!(workspace_paths.project_db.is_file());
        assert!(workspace_paths.workspace_db.is_file());
        assert!(workspace_paths.index_db.is_file());

        let registry = GlobalRegistry::open(&paths.global_db).expect("registry should open");
        assert!(
            registry
                .get_project(outcome.project_id)
                .expect("lookup should succeed")
                .is_some()
        );
        assert!(
            registry
                .get_workspace(outcome.workspace_id)
                .expect("lookup should succeed")
                .is_some()
        );
    }

    #[test]
    fn non_git_workspace_root_is_the_requested_directory() {
        let global_home = TestDir::create("non-git-root-global");
        let workspace = TestDir::create("non-git-root-workspace");
        let paths = global_paths(&global_home);

        let outcome = init_fresh_workspace(workspace.path(), &paths).expect("init should succeed");
        assert_eq!(
            outcome.workspace_root,
            fs::canonicalize(workspace.path()).expect("workspace path should canonicalize")
        );
    }

    #[test]
    fn git_workspace_root_resolves_to_repository_root_not_subdirectory() {
        let global_home = TestDir::create("git-subdir-global");
        let workspace = TestDir::create("git-subdir-workspace");
        make_git_repo(&workspace);
        let nested = workspace.path().join("crates").join("core");
        fs::create_dir_all(&nested).expect("nested dir should be created");
        let paths = global_paths(&global_home);

        let outcome =
            init_fresh_workspace(&nested, &paths).expect("init from nested dir should succeed");
        assert_eq!(
            outcome.workspace_root,
            fs::canonicalize(workspace.path()).expect("workspace path should canonicalize")
        );
    }

    #[test]
    fn reinit_same_workspace_is_idempotent() {
        let global_home = TestDir::create("idempotent-global");
        let workspace = TestDir::create("idempotent-workspace");
        make_git_repo(&workspace);
        let paths = global_paths(&global_home);

        let first =
            init_fresh_workspace(workspace.path(), &paths).expect("first init should succeed");
        assert!(first.freshly_created);

        let second =
            init_fresh_workspace(workspace.path(), &paths).expect("second init should succeed");
        assert!(!second.freshly_created);
        assert_eq!(second.project_id, first.project_id);
        assert_eq!(second.workspace_id, first.workspace_id);
    }

    #[test]
    fn malformed_workspace_toml_is_rejected() {
        let global_home = TestDir::create("malformed-global");
        let workspace = TestDir::create("malformed-workspace");
        make_git_repo(&workspace);
        let paths = global_paths(&global_home);

        let canonical =
            fs::canonicalize(workspace.path()).expect("workspace path should canonicalize");
        let workspace_paths = WorkspacePaths::from_root(&canonical);
        fs::create_dir_all(&workspace_paths.root).expect("root should be created");
        fs::write(&workspace_paths.identity_file, "not [ valid toml")
            .expect("fixture should be written");

        let error = init_fresh_workspace(workspace.path(), &paths)
            .expect_err("malformed identity must be rejected");
        assert!(matches!(error, InitError::MalformedIdentity { .. }));
    }

    #[test]
    fn project_db_identity_mismatch_is_rejected() {
        let global_home = TestDir::create("project-mismatch-global");
        let workspace = TestDir::create("project-mismatch-workspace");
        make_git_repo(&workspace);
        let paths = global_paths(&global_home);

        let outcome =
            init_fresh_workspace(workspace.path(), &paths).expect("first init should succeed");
        let workspace_paths = WorkspacePaths::from_root(&outcome.workspace_root);

        {
            let project_db = schema::project::open(&workspace_paths.project_db)
                .expect("project.db should reopen");
            let other_project = ProjectId::generate();
            project_db
                .connection
                .execute(
                    "UPDATE db_meta SET project_uid = ?1 WHERE id = 0",
                    params![other_project.to_bytes().to_vec()],
                )
                .expect("tampering update should succeed");
        }

        let error = init_fresh_workspace(workspace.path(), &paths)
            .expect_err("mismatched project.db identity must be rejected");
        assert!(matches!(error, InitError::ProjectIdentityMismatch { .. }));
    }

    #[test]
    fn workspace_db_identity_mismatch_is_rejected() {
        let global_home = TestDir::create("workspace-mismatch-global");
        let workspace = TestDir::create("workspace-mismatch-workspace");
        make_git_repo(&workspace);
        let paths = global_paths(&global_home);

        let outcome =
            init_fresh_workspace(workspace.path(), &paths).expect("first init should succeed");
        let workspace_paths = WorkspacePaths::from_root(&outcome.workspace_root);

        {
            let workspace_db = schema::workspace::open(&workspace_paths.workspace_db)
                .expect("workspace.db should reopen");
            let other_workspace = WorkspaceId::generate();
            workspace_db
                .connection
                .execute(
                    "UPDATE db_meta SET workspace_uid = ?1 WHERE id = 0",
                    params![other_workspace.to_bytes().to_vec()],
                )
                .expect("tampering update should succeed");
        }

        let error = init_fresh_workspace(workspace.path(), &paths)
            .expect_err("mismatched workspace.db identity must be rejected");
        assert!(matches!(error, InitError::WorkspaceIdentityMismatch { .. }));
    }

    #[test]
    fn copied_identity_at_a_new_locator_is_rejected_by_the_registry() {
        let global_home = TestDir::create("copy-global");
        let original = TestDir::create("copy-original");
        make_git_repo(&original);
        let paths = global_paths(&global_home);

        let outcome =
            init_fresh_workspace(original.path(), &paths).expect("original init should succeed");

        // Simulate copying `.brainprint/workspace.toml` (but not the DBs)
        // to a brand new directory: same identity, different locator.
        let copy = TestDir::create("copy-target");
        make_git_repo(&copy);
        let copy_canonical = fs::canonicalize(copy.path()).expect("copy path should canonicalize");
        let copy_workspace_paths = WorkspacePaths::from_root(&copy_canonical);
        let original_workspace_paths = WorkspacePaths::from_root(&outcome.workspace_root);
        fs::create_dir_all(&copy_workspace_paths.root).expect("root should be created");
        fs::copy(
            &original_workspace_paths.identity_file,
            &copy_workspace_paths.identity_file,
        )
        .expect("identity file should copy");

        let error = init_fresh_workspace(copy.path(), &paths)
            .expect_err("same identity at a different locator must be rejected");
        assert!(matches!(error, InitError::Registry(_)));
    }

    #[test]
    fn db_creation_failure_leaves_no_registry_trace() {
        let global_home = TestDir::create("db-failure-global");
        let workspace = TestDir::create("db-failure-workspace");
        make_git_repo(&workspace);
        let paths = global_paths(&global_home);

        let canonical =
            fs::canonicalize(workspace.path()).expect("workspace path should canonicalize");
        let workspace_paths = WorkspacePaths::from_root(&canonical);
        fs::create_dir_all(&workspace_paths.root).expect("root should be created");
        // Block project.db's parent directory with a plain file so DB open fails.
        fs::write(&workspace_paths.data_dir, b"not a directory")
            .expect("blocking file should be written");

        let error = init_fresh_workspace(workspace.path(), &paths)
            .expect_err("blocked data dir must fail init");
        assert!(matches!(error, InitError::Db(_)));

        // The identity file was written before the failure, but the
        // registry -- which comes last -- must never have been touched.
        let identity = read_identity_file(&workspace_paths.identity_file)
            .expect("identity read should succeed")
            .expect("identity should exist despite the later failure");
        let registry = GlobalRegistry::open(&paths.global_db).expect("registry should open");
        assert!(
            registry
                .get_project(identity.project_id)
                .expect("lookup should succeed")
                .is_none()
        );
    }

    #[test]
    fn source_files_are_not_touched() {
        let global_home = TestDir::create("source-safe-global");
        let workspace = TestDir::create("source-safe-workspace");
        make_git_repo(&workspace);
        let paths = global_paths(&global_home);

        let source_path = workspace.path().join("src").join("main.rs");
        fs::create_dir_all(source_path.parent().expect("src path should have a parent"))
            .expect("src dir should be created");
        let original_content = "fn main() {}\n";
        fs::write(&source_path, original_content).expect("source fixture should be written");

        init_fresh_workspace(workspace.path(), &paths).expect("init should succeed");

        let after = fs::read_to_string(&source_path).expect("source file should still be readable");
        assert_eq!(after, original_content);
    }

    #[test]
    fn git_worktree_style_git_file_is_treated_as_an_independent_fresh_case() {
        let global_home = TestDir::create("worktree-style-global");
        let workspace = TestDir::create("worktree-style-workspace");
        // Simulate a `git worktree add` checkout: `.git` is a FILE
        // pointing elsewhere, not a directory. Task 6 does not attempt to
        // detect or link this to another Project -- that is task 7's job
        // -- so this must succeed as an ordinary fresh init.
        fs::write(
            workspace.path().join(".git"),
            "gitdir: /somewhere/else/.git/worktrees/example\n",
        )
        .expect(".git file fixture should be written");
        let paths = global_paths(&global_home);

        let outcome = init_fresh_workspace(workspace.path(), &paths)
            .expect("worktree-style init should succeed");
        assert!(outcome.is_git);
        assert!(outcome.freshly_created);
    }
}
