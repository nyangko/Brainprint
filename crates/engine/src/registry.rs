//! `global.db` Project/Workspace registry: identity → locator lookup.
//!
//! Scope (see #15 task 4 / #13 task 3 §4 and task 6 §2): this module owns
//! only `project_registry` and `workspace_registry` — the identity/locator
//! index global.db uses to find a Project's home store and a Workspace's
//! current root. It has no knowledge of:
//! - project.db/workspace.db/index.db domain schema (#15 task 5)
//! - fresh/secondary Workspace init orchestration (#15 task 6, 7)
//! - daemon connection ownership (#15 task 9)
//!
//! Core identity rule (#13 task 3 §15, task 5 §4-5): a locator is mutable
//! position information, never identity. `register_*` is a create-only
//! operation — if the identity already exists with a *different* locator,
//! that is a conflict (possible copy/duplicate) and is rejected rather than
//! silently merged. An explicit `update_*_locator` call is required to move
//! an existing identity to a new locator. False merge is treated as more
//! dangerous than false split.

use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

use brainprint_core::{ProjectId, WorkspaceId};
use rusqlite::{Connection, OptionalExtension, params};

use crate::db::{self, DbKind, DbOpenError, Migration};

const GLOBAL_MIGRATIONS: &[Migration] = &[Migration {
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
}];

/// A registered Project: its stable identity and current project-home locator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRegistryEntry {
    pub project_id: ProjectId,
    pub home_locator: PathBuf,
    pub created_at: String,
    pub updated_at: String,
}

/// A registered Workspace: its stable identity, owning Project, and current locator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRegistryEntry {
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub locator: PathBuf,
    /// Whether this Workspace is its Project's current project-home
    /// (the sole owner of that Project's `project.db`; see #13 task 5 §4).
    pub is_project_home: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// Candidates found for a given locator. A locator is not a stable
/// identity, so more than one entry (or none) is an expected outcome, not
/// an error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocatorCandidates {
    pub projects: Vec<ProjectRegistryEntry>,
    pub workspaces: Vec<WorkspaceRegistryEntry>,
}

/// Failure registering, updating, or looking up global registry entries.
#[derive(Debug)]
pub enum RegistryError {
    Open(DbOpenError),
    Sqlite(rusqlite::Error),
    /// A Workspace was registered against a ProjectID with no registry entry.
    UnknownProject {
        project_id: ProjectId,
    },
    /// `update_project_locator` targeted a ProjectID with no registry entry.
    UnknownWorkspace {
        workspace_id: WorkspaceId,
    },
    /// `register_project` was called for an already-registered ProjectID
    /// with a locator that does not match the stored one.
    ProjectLocatorConflict {
        project_id: ProjectId,
        existing_locator: PathBuf,
        requested_locator: PathBuf,
    },
    /// `register_workspace` was called for an already-registered
    /// WorkspaceID whose stored (project, locator) does not match the
    /// request — including the same WorkspaceID observed at two different
    /// locators, which must never be silently merged.
    WorkspaceIdentityConflict {
        workspace_id: WorkspaceId,
        existing_project_id: ProjectId,
        existing_locator: PathBuf,
        requested_project_id: ProjectId,
        requested_locator: PathBuf,
    },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(source) => write!(formatter, "failed to open global registry: {source}"),
            Self::Sqlite(source) => write!(formatter, "global registry sqlite error: {source}"),
            Self::UnknownProject { project_id } => {
                write!(formatter, "no registry entry for project {project_id}")
            }
            Self::UnknownWorkspace { workspace_id } => {
                write!(formatter, "no registry entry for workspace {workspace_id}")
            }
            Self::ProjectLocatorConflict {
                project_id,
                existing_locator,
                requested_locator,
            } => write!(
                formatter,
                "project {project_id} is already registered at {} (requested {})",
                existing_locator.display(),
                requested_locator.display()
            ),
            Self::WorkspaceIdentityConflict {
                workspace_id,
                existing_project_id,
                existing_locator,
                requested_project_id,
                requested_locator,
            } => write!(
                formatter,
                "workspace {workspace_id} is already registered as project {existing_project_id} at {} \
                 (requested project {requested_project_id} at {})",
                existing_locator.display(),
                requested_locator.display()
            ),
        }
    }
}

impl Error for RegistryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Open(source) => Some(source),
            Self::Sqlite(source) => Some(source),
            Self::UnknownProject { .. }
            | Self::UnknownWorkspace { .. }
            | Self::ProjectLocatorConflict { .. }
            | Self::WorkspaceIdentityConflict { .. } => None,
        }
    }
}

impl From<DbOpenError> for RegistryError {
    fn from(source: DbOpenError) -> Self {
        Self::Open(source)
    }
}

impl From<rusqlite::Error> for RegistryError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

/// Handle to `global.db`'s Project/Workspace identity registry.
pub struct GlobalRegistry {
    connection: Connection,
}

impl GlobalRegistry {
    /// Open (creating and migrating if needed) the registry at `path`.
    pub fn open(path: &Path) -> Result<Self, RegistryError> {
        let opened = db::open(path, DbKind::Global, GLOBAL_MIGRATIONS)?;
        Ok(Self {
            connection: opened.connection,
        })
    }

    /// Register a new Project, or confirm an unchanged re-registration.
    ///
    /// Create-only: if `project_id` is already registered at a *different*
    /// `home_locator`, this returns [`RegistryError::ProjectLocatorConflict`]
    /// rather than moving it. Use [`Self::update_project_locator`] for a
    /// deliberate relocation.
    pub fn register_project(
        &self,
        project_id: ProjectId,
        home_locator: &Path,
    ) -> Result<ProjectRegistryEntry, RegistryError> {
        if let Some(existing) = self.get_project(project_id)? {
            if existing.home_locator == home_locator {
                return Ok(existing);
            }
            return Err(RegistryError::ProjectLocatorConflict {
                project_id,
                existing_locator: existing.home_locator,
                requested_locator: home_locator.to_path_buf(),
            });
        }

        let now = db::now_millis_text();
        self.connection.execute(
            "INSERT INTO project_registry (project_uid, home_locator, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?3)",
            params![
                project_id.to_bytes().to_vec(),
                locator_to_text(home_locator),
                now
            ],
        )?;

        Ok(ProjectRegistryEntry {
            project_id,
            home_locator: home_locator.to_path_buf(),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// Look up a Project by its stable identity.
    pub fn get_project(
        &self,
        project_id: ProjectId,
    ) -> Result<Option<ProjectRegistryEntry>, RegistryError> {
        self.connection
            .query_row(
                "SELECT home_locator, created_at, updated_at FROM project_registry WHERE project_uid = ?1",
                params![project_id.to_bytes().to_vec()],
                |row| {
                    Ok(ProjectRegistryEntry {
                        project_id,
                        home_locator: PathBuf::from(row.get::<_, String>(0)?),
                        created_at: row.get(1)?,
                        updated_at: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(RegistryError::from)
    }

    /// Explicitly relocate an already-registered Project's home locator,
    /// keeping its identity unchanged (#13 task 5 §5).
    pub fn update_project_locator(
        &self,
        project_id: ProjectId,
        new_locator: &Path,
    ) -> Result<ProjectRegistryEntry, RegistryError> {
        let changed = self.connection.execute(
            "UPDATE project_registry SET home_locator = ?1, updated_at = ?2 WHERE project_uid = ?3",
            params![
                locator_to_text(new_locator),
                db::now_millis_text(),
                project_id.to_bytes().to_vec()
            ],
        )?;
        if changed == 0 {
            return Err(RegistryError::UnknownProject { project_id });
        }

        self.get_project(project_id)?
            .ok_or(RegistryError::UnknownProject { project_id })
    }

    /// Register a new Workspace under an already-registered Project, or
    /// confirm an unchanged re-registration.
    ///
    /// Create-only, mirroring [`Self::register_project`]: if `workspace_id`
    /// is already registered under a different Project or a different
    /// locator, this returns [`RegistryError::WorkspaceIdentityConflict`]
    /// instead of merging — the same WorkspaceID found at two different
    /// active locators must never be silently treated as one move.
    pub fn register_workspace(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        locator: &Path,
        is_project_home: bool,
    ) -> Result<WorkspaceRegistryEntry, RegistryError> {
        if self.get_project(project_id)?.is_none() {
            return Err(RegistryError::UnknownProject { project_id });
        }

        if let Some(existing) = self.get_workspace(workspace_id)? {
            if existing.project_id == project_id && existing.locator == locator {
                return Ok(existing);
            }
            return Err(RegistryError::WorkspaceIdentityConflict {
                workspace_id,
                existing_project_id: existing.project_id,
                existing_locator: existing.locator,
                requested_project_id: project_id,
                requested_locator: locator.to_path_buf(),
            });
        }

        let now = db::now_millis_text();
        self.connection.execute(
            "INSERT INTO workspace_registry \
             (workspace_uid, project_uid, locator, is_project_home, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![
                workspace_id.to_bytes().to_vec(),
                project_id.to_bytes().to_vec(),
                locator_to_text(locator),
                is_project_home,
                now
            ],
        )?;

        Ok(WorkspaceRegistryEntry {
            workspace_id,
            project_id,
            locator: locator.to_path_buf(),
            is_project_home,
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// Look up a Workspace by its stable identity.
    pub fn get_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<WorkspaceRegistryEntry>, RegistryError> {
        self.connection
            .query_row(
                "SELECT project_uid, locator, is_project_home, created_at, updated_at \
                 FROM workspace_registry WHERE workspace_uid = ?1",
                params![workspace_id.to_bytes().to_vec()],
                |row| {
                    Ok(WorkspaceRegistryEntry {
                        workspace_id,
                        project_id: project_id_from_blob(row.get(0)?),
                        locator: PathBuf::from(row.get::<_, String>(1)?),
                        is_project_home: row.get(2)?,
                        created_at: row.get(3)?,
                        updated_at: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(RegistryError::from)
    }

    /// Explicitly relocate an already-registered Workspace's locator,
    /// keeping its identity unchanged.
    pub fn update_workspace_locator(
        &self,
        workspace_id: WorkspaceId,
        new_locator: &Path,
    ) -> Result<WorkspaceRegistryEntry, RegistryError> {
        let changed = self.connection.execute(
            "UPDATE workspace_registry SET locator = ?1, updated_at = ?2 WHERE workspace_uid = ?3",
            params![
                locator_to_text(new_locator),
                db::now_millis_text(),
                workspace_id.to_bytes().to_vec()
            ],
        )?;
        if changed == 0 {
            return Err(RegistryError::UnknownWorkspace { workspace_id });
        }

        self.get_workspace(workspace_id)?
            .ok_or(RegistryError::UnknownWorkspace { workspace_id })
    }

    /// Find every registered Project/Workspace currently pointing at `locator`.
    ///
    /// A locator is mutable position information, not identity, so this
    /// returns candidates rather than assuming at most one match.
    pub fn find_by_locator(&self, locator: &Path) -> Result<LocatorCandidates, RegistryError> {
        let text = locator_to_text(locator);

        let mut project_statement = self.connection.prepare(
            "SELECT project_uid, home_locator, created_at, updated_at \
             FROM project_registry WHERE home_locator = ?1",
        )?;
        let projects = project_statement
            .query_map(params![text], |row| {
                Ok(ProjectRegistryEntry {
                    project_id: project_id_from_blob(row.get(0)?),
                    home_locator: PathBuf::from(row.get::<_, String>(1)?),
                    created_at: row.get(2)?,
                    updated_at: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut workspace_statement = self.connection.prepare(
            "SELECT workspace_uid, project_uid, is_project_home, created_at, updated_at \
             FROM workspace_registry WHERE locator = ?1",
        )?;
        let workspaces = workspace_statement
            .query_map(params![text], |row| {
                Ok(WorkspaceRegistryEntry {
                    workspace_id: workspace_id_from_blob(row.get(0)?),
                    project_id: project_id_from_blob(row.get(1)?),
                    locator: locator.to_path_buf(),
                    is_project_home: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(LocatorCandidates {
            projects,
            workspaces,
        })
    }
}

fn locator_to_text(locator: &Path) -> String {
    locator.to_string_lossy().into_owned()
}

fn project_id_from_blob(bytes: Vec<u8>) -> ProjectId {
    let array: [u8; 16] = bytes
        .as_slice()
        .try_into()
        .expect("project_uid column must always hold a 16-byte value written by this module");
    ProjectId::from_bytes(array)
}

fn workspace_id_from_blob(bytes: Vec<u8>) -> WorkspaceId {
    let array: [u8; 16] = bytes
        .as_slice()
        .try_into()
        .expect("workspace_uid column must always hold a 16-byte value written by this module");
    WorkspaceId::from_bytes(array)
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
                "brainprint-registry-{label}-{}-{sequence}",
                process::id()
            ));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self(path)
        }

        fn db_path(&self) -> PathBuf {
            self.0.join("data").join("global.db")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn project_registers_and_is_retrievable() {
        let dir = TestDir::create("project-register");
        let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");
        let project_id = ProjectId::generate();
        let home = PathBuf::from("/repo/main");

        let entry = registry
            .register_project(project_id, &home)
            .expect("new project should register");
        assert_eq!(entry.project_id, project_id);
        assert_eq!(entry.home_locator, home);

        let fetched = registry
            .get_project(project_id)
            .expect("lookup should succeed")
            .expect("project should be found");
        assert_eq!(fetched, entry);
    }

    #[test]
    fn workspace_registers_and_is_retrievable() {
        let dir = TestDir::create("workspace-register");
        let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");
        let project_id = ProjectId::generate();
        let workspace_id = WorkspaceId::generate();
        let home = PathBuf::from("/repo/main");

        registry
            .register_project(project_id, &home)
            .expect("project should register");
        let entry = registry
            .register_workspace(workspace_id, project_id, &home, true)
            .expect("workspace should register");
        assert!(entry.is_project_home);

        let fetched = registry
            .get_workspace(workspace_id)
            .expect("lookup should succeed")
            .expect("workspace should be found");
        assert_eq!(fetched, entry);
    }

    #[test]
    fn one_project_accepts_multiple_distinct_workspaces() {
        let dir = TestDir::create("multi-workspace");
        let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");
        let project_id = ProjectId::generate();
        registry
            .register_project(project_id, Path::new("/repo/main"))
            .expect("project should register");

        let home_workspace = registry
            .register_workspace(
                WorkspaceId::generate(),
                project_id,
                Path::new("/repo/main"),
                true,
            )
            .expect("home workspace should register");
        let worktree_workspace = registry
            .register_workspace(
                WorkspaceId::generate(),
                project_id,
                Path::new("/repo/worktrees/feature-x"),
                false,
            )
            .expect("secondary worktree should register");

        assert_ne!(home_workspace.workspace_id, worktree_workspace.workspace_id);
        assert_eq!(home_workspace.project_id, worktree_workspace.project_id);
        assert!(home_workspace.is_project_home);
        assert!(!worktree_workspace.is_project_home);
    }

    #[test]
    fn locator_update_preserves_identity() {
        let dir = TestDir::create("locator-move");
        let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");
        let project_id = ProjectId::generate();
        let workspace_id = WorkspaceId::generate();
        registry
            .register_project(project_id, Path::new("/repo/main"))
            .expect("project should register");
        registry
            .register_workspace(workspace_id, project_id, Path::new("/repo/main"), true)
            .expect("workspace should register");

        let moved_project = registry
            .update_project_locator(project_id, Path::new("/repo/moved"))
            .expect("project relocation should succeed");
        assert_eq!(moved_project.project_id, project_id);
        assert_eq!(moved_project.home_locator, PathBuf::from("/repo/moved"));

        let moved_workspace = registry
            .update_workspace_locator(workspace_id, Path::new("/repo/moved"))
            .expect("workspace relocation should succeed");
        assert_eq!(moved_workspace.workspace_id, workspace_id);
        assert_eq!(moved_workspace.locator, PathBuf::from("/repo/moved"));
    }

    #[test]
    fn locator_lookup_returns_project_and_workspace_candidates() {
        let dir = TestDir::create("locator-lookup");
        let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");
        let project_id = ProjectId::generate();
        let workspace_id = WorkspaceId::generate();
        let home = Path::new("/repo/main");
        registry
            .register_project(project_id, home)
            .expect("project should register");
        registry
            .register_workspace(workspace_id, project_id, home, true)
            .expect("workspace should register");

        let candidates = registry
            .find_by_locator(home)
            .expect("lookup should succeed");
        assert_eq!(candidates.projects.len(), 1);
        assert_eq!(candidates.projects[0].project_id, project_id);
        assert_eq!(candidates.workspaces.len(), 1);
        assert_eq!(candidates.workspaces[0].workspace_id, workspace_id);

        let empty = registry
            .find_by_locator(Path::new("/nowhere"))
            .expect("lookup of unknown locator should succeed with no candidates");
        assert!(empty.projects.is_empty());
        assert!(empty.workspaces.is_empty());
    }

    #[test]
    fn workspace_registration_rejects_unknown_project() {
        let dir = TestDir::create("unknown-project");
        let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");
        let unregistered_project = ProjectId::generate();

        let error = registry
            .register_workspace(
                WorkspaceId::generate(),
                unregistered_project,
                Path::new("/repo/main"),
                true,
            )
            .expect_err("workspace under an unregistered project must be rejected");

        assert!(matches!(
            error,
            RegistryError::UnknownProject {
                project_id
            } if project_id == unregistered_project
        ));
    }

    #[test]
    fn duplicate_project_registration_is_idempotent_for_matching_locator() {
        let dir = TestDir::create("duplicate-project-same");
        let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");
        let project_id = ProjectId::generate();
        let home = Path::new("/repo/main");

        let first = registry
            .register_project(project_id, home)
            .expect("first register should succeed");
        let second = registry
            .register_project(project_id, home)
            .expect("re-registering with the same locator should be idempotent");

        assert_eq!(first, second);
    }

    #[test]
    fn duplicate_project_registration_with_new_locator_is_rejected() {
        let dir = TestDir::create("duplicate-project-conflict");
        let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");
        let project_id = ProjectId::generate();
        registry
            .register_project(project_id, Path::new("/repo/main"))
            .expect("first register should succeed");

        let error = registry
            .register_project(project_id, Path::new("/repo/copy"))
            .expect_err("re-registering with a different locator must be rejected, not merged");

        assert!(matches!(
            error,
            RegistryError::ProjectLocatorConflict { project_id: id, .. } if id == project_id
        ));
    }

    #[test]
    fn duplicate_workspace_registration_with_new_locator_is_rejected() {
        let dir = TestDir::create("duplicate-workspace-conflict");
        let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");
        let project_id = ProjectId::generate();
        let workspace_id = WorkspaceId::generate();
        registry
            .register_project(project_id, Path::new("/repo/main"))
            .expect("project should register");
        registry
            .register_workspace(workspace_id, project_id, Path::new("/repo/main"), true)
            .expect("first registration should succeed");

        let error = registry
            .register_workspace(workspace_id, project_id, Path::new("/repo/copy"), true)
            .expect_err("same WorkspaceID at a different locator must never be silently merged");

        assert!(matches!(
            error,
            RegistryError::WorkspaceIdentityConflict { workspace_id: id, .. } if id == workspace_id
        ));

        // The original registration must remain unchanged.
        let unchanged = registry
            .get_workspace(workspace_id)
            .expect("lookup should succeed")
            .expect("workspace should still be registered");
        assert_eq!(unchanged.locator, PathBuf::from("/repo/main"));
    }

    #[test]
    fn registry_survives_reopen() {
        let dir = TestDir::create("reopen");
        let project_id = ProjectId::generate();
        let workspace_id = WorkspaceId::generate();
        {
            let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");
            registry
                .register_project(project_id, Path::new("/repo/main"))
                .expect("project should register");
            registry
                .register_workspace(workspace_id, project_id, Path::new("/repo/main"), true)
                .expect("workspace should register");
        }

        let reopened = GlobalRegistry::open(&dir.db_path()).expect("registry should reopen");
        let project = reopened
            .get_project(project_id)
            .expect("lookup should succeed")
            .expect("project should still be registered after reopen");
        let workspace = reopened
            .get_workspace(workspace_id)
            .expect("lookup should succeed")
            .expect("workspace should still be registered after reopen");

        assert_eq!(project.project_id, project_id);
        assert_eq!(workspace.workspace_id, workspace_id);
        assert_eq!(workspace.project_id, project_id);
    }
}
