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

const GLOBAL_MIGRATIONS: &[Migration] = &[
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
];

/// Whether the registry believes this identity's locator is currently
/// reachable/valid (#13 task 6 §2, #15 task 11). I1 has no background
/// scanner -- this is only ever set as a direct result of an explicit
/// reopen/recovery observation (see `brainprint-engine::init`), never
/// inferred from a locator string alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryState {
    Active,
    Missing,
}

impl RegistryState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "ACTIVE",
            Self::Missing => "MISSING",
        }
    }

    fn parse(raw: &str) -> Result<Self, RegistryError> {
        match raw {
            "ACTIVE" => Ok(Self::Active),
            "MISSING" => Ok(Self::Missing),
            other => Err(RegistryError::UnknownState {
                raw: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for RegistryState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A registered Project: its stable identity and current project-home locator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRegistryEntry {
    pub project_id: ProjectId,
    pub home_locator: PathBuf,
    pub state: RegistryState,
    /// Millisecond-epoch text of the last successful observation. `None`
    /// only for a row created before this column existed (never written
    /// by this module).
    pub last_seen_at: Option<String>,
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
    pub state: RegistryState,
    pub last_seen_at: Option<String>,
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
    /// `register_git_lineage` was called for a `git_common_dir` already
    /// mapped to a *different* Project. The common Git directory is
    /// canonical repository-lineage evidence (#15 task 7), so two
    /// different Projects claiming the same one is a conflict, never a
    /// silent merge.
    GitLineageConflict {
        git_common_dir: PathBuf,
        existing_project_id: ProjectId,
        requested_project_id: ProjectId,
    },
    /// A `state` column held a value this module never writes.
    UnknownState {
        raw: String,
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
            Self::GitLineageConflict {
                git_common_dir,
                existing_project_id,
                requested_project_id,
            } => write!(
                formatter,
                "git common directory {} is already linked to project {existing_project_id} \
                 (requested project {requested_project_id})",
                git_common_dir.display()
            ),
            Self::UnknownState { raw } => write!(formatter, "unknown registry state {raw:?}"),
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
            | Self::WorkspaceIdentityConflict { .. }
            | Self::GitLineageConflict { .. }
            | Self::UnknownState { .. } => None,
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
    ///
    /// A matching re-registration is treated as a fresh observation (#15
    /// task 11): it marks the entry [`RegistryState::Active`] and touches
    /// `last_seen_at`, so a Project previously marked
    /// [`RegistryState::Missing`] recovers automatically the next time it
    /// is genuinely seen again.
    pub fn register_project(
        &self,
        project_id: ProjectId,
        home_locator: &Path,
    ) -> Result<ProjectRegistryEntry, RegistryError> {
        if let Some(existing) = self.get_project(project_id)? {
            if existing.home_locator == home_locator {
                return self.mark_project_active(project_id);
            }
            return Err(RegistryError::ProjectLocatorConflict {
                project_id,
                existing_locator: existing.home_locator,
                requested_locator: home_locator.to_path_buf(),
            });
        }

        let now = db::now_millis_text();
        self.connection.execute(
            "INSERT INTO project_registry \
             (project_uid, home_locator, state, last_seen_at, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?4, ?4)",
            params![
                project_id.to_bytes().to_vec(),
                locator_to_text(home_locator),
                RegistryState::Active.as_str(),
                now
            ],
        )?;

        Ok(ProjectRegistryEntry {
            project_id,
            home_locator: home_locator.to_path_buf(),
            state: RegistryState::Active,
            last_seen_at: Some(now.clone()),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// Mark a Project as currently observed/reachable, touching
    /// `last_seen_at` (#15 task 11).
    pub fn mark_project_active(
        &self,
        project_id: ProjectId,
    ) -> Result<ProjectRegistryEntry, RegistryError> {
        let now = db::now_millis_text();
        let changed = self.connection.execute(
            "UPDATE project_registry SET state = ?1, last_seen_at = ?2, updated_at = ?2 \
             WHERE project_uid = ?3",
            params![
                RegistryState::Active.as_str(),
                now,
                project_id.to_bytes().to_vec()
            ],
        )?;
        if changed == 0 {
            return Err(RegistryError::UnknownProject { project_id });
        }
        self.get_project(project_id)?
            .ok_or(RegistryError::UnknownProject { project_id })
    }

    /// Mark a Project's home locator as not currently reachable (#15 task
    /// 11: e.g. the project-home `project.db` is missing). `last_seen_at`
    /// is left untouched -- it keeps recording the last time the Project
    /// genuinely *was* seen, not this negative observation.
    pub fn mark_project_missing(
        &self,
        project_id: ProjectId,
    ) -> Result<ProjectRegistryEntry, RegistryError> {
        let changed = self.connection.execute(
            "UPDATE project_registry SET state = ?1, updated_at = ?2 WHERE project_uid = ?3",
            params![
                RegistryState::Missing.as_str(),
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

    /// Look up a Project by its stable identity.
    pub fn get_project(
        &self,
        project_id: ProjectId,
    ) -> Result<Option<ProjectRegistryEntry>, RegistryError> {
        self.connection
            .query_row(
                "SELECT home_locator, state, last_seen_at, created_at, updated_at \
                 FROM project_registry WHERE project_uid = ?1",
                params![project_id.to_bytes().to_vec()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?
            .map(
                |(home_locator, state_raw, last_seen_at, created_at, updated_at)| {
                    Ok(ProjectRegistryEntry {
                        project_id,
                        home_locator: PathBuf::from(home_locator),
                        state: RegistryState::parse(&state_raw)?,
                        last_seen_at,
                        created_at,
                        updated_at,
                    })
                },
            )
            .transpose()
    }

    /// Explicitly relocate an already-registered Project's home locator,
    /// keeping its identity unchanged (#13 task 5 §5). Also marks the
    /// entry [`RegistryState::Active`] with a fresh `last_seen_at`: a
    /// deliberate relocation is itself a confirmed-live observation (#15
    /// task 11's directory-move repair path).
    pub fn update_project_locator(
        &self,
        project_id: ProjectId,
        new_locator: &Path,
    ) -> Result<ProjectRegistryEntry, RegistryError> {
        let now = db::now_millis_text();
        let changed = self.connection.execute(
            "UPDATE project_registry \
             SET home_locator = ?1, state = ?2, last_seen_at = ?3, updated_at = ?3 \
             WHERE project_uid = ?4",
            params![
                locator_to_text(new_locator),
                RegistryState::Active.as_str(),
                now,
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
    ///
    /// A matching re-registration marks the entry [`RegistryState::Active`]
    /// with a fresh `last_seen_at` (#15 task 11), same as
    /// [`Self::register_project`].
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
                return self.mark_workspace_active(workspace_id);
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
             (workspace_uid, project_uid, locator, is_project_home, state, last_seen_at, \
              created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?6)",
            params![
                workspace_id.to_bytes().to_vec(),
                project_id.to_bytes().to_vec(),
                locator_to_text(locator),
                is_project_home,
                RegistryState::Active.as_str(),
                now
            ],
        )?;

        Ok(WorkspaceRegistryEntry {
            workspace_id,
            project_id,
            locator: locator.to_path_buf(),
            is_project_home,
            state: RegistryState::Active,
            last_seen_at: Some(now.clone()),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// Mark a Workspace as currently observed/reachable, touching
    /// `last_seen_at` (#15 task 11).
    pub fn mark_workspace_active(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<WorkspaceRegistryEntry, RegistryError> {
        let now = db::now_millis_text();
        let changed = self.connection.execute(
            "UPDATE workspace_registry SET state = ?1, last_seen_at = ?2, updated_at = ?2 \
             WHERE workspace_uid = ?3",
            params![
                RegistryState::Active.as_str(),
                now,
                workspace_id.to_bytes().to_vec()
            ],
        )?;
        if changed == 0 {
            return Err(RegistryError::UnknownWorkspace { workspace_id });
        }
        self.get_workspace(workspace_id)?
            .ok_or(RegistryError::UnknownWorkspace { workspace_id })
    }

    /// Mark a Workspace's locator as not currently reachable (#15 task
    /// 11). `last_seen_at` is left untouched.
    pub fn mark_workspace_missing(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<WorkspaceRegistryEntry, RegistryError> {
        let changed = self.connection.execute(
            "UPDATE workspace_registry SET state = ?1, updated_at = ?2 WHERE workspace_uid = ?3",
            params![
                RegistryState::Missing.as_str(),
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

    /// Look up a Workspace by its stable identity.
    pub fn get_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<WorkspaceRegistryEntry>, RegistryError> {
        self.connection
            .query_row(
                "SELECT project_uid, locator, is_project_home, state, last_seen_at, \
                        created_at, updated_at \
                 FROM workspace_registry WHERE workspace_uid = ?1",
                params![workspace_id.to_bytes().to_vec()],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, bool>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?
            .map(
                |(
                    project_uid,
                    locator,
                    is_project_home,
                    state_raw,
                    last_seen_at,
                    created_at,
                    updated_at,
                )| {
                    Ok(WorkspaceRegistryEntry {
                        workspace_id,
                        project_id: project_id_from_blob(project_uid),
                        locator: PathBuf::from(locator),
                        is_project_home,
                        state: RegistryState::parse(&state_raw)?,
                        last_seen_at,
                        created_at,
                        updated_at,
                    })
                },
            )
            .transpose()
    }

    /// Explicitly relocate an already-registered Workspace's locator,
    /// keeping its identity unchanged. Also marks the entry
    /// [`RegistryState::Active`] with a fresh `last_seen_at` (#15 task 11's
    /// directory-move repair path).
    pub fn update_workspace_locator(
        &self,
        workspace_id: WorkspaceId,
        new_locator: &Path,
    ) -> Result<WorkspaceRegistryEntry, RegistryError> {
        let now = db::now_millis_text();
        let changed = self.connection.execute(
            "UPDATE workspace_registry \
             SET locator = ?1, state = ?2, last_seen_at = ?3, updated_at = ?3 \
             WHERE workspace_uid = ?4",
            params![
                locator_to_text(new_locator),
                RegistryState::Active.as_str(),
                now,
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
            "SELECT project_uid, home_locator, state, last_seen_at, created_at, updated_at \
             FROM project_registry WHERE home_locator = ?1",
        )?;
        let projects = project_statement
            .query_map(params![text], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(
                |(project_uid, home_locator, state_raw, last_seen_at, created_at, updated_at)| {
                    Ok(ProjectRegistryEntry {
                        project_id: project_id_from_blob(project_uid),
                        home_locator: PathBuf::from(home_locator),
                        state: RegistryState::parse(&state_raw)?,
                        last_seen_at,
                        created_at,
                        updated_at,
                    })
                },
            )
            .collect::<Result<Vec<_>, RegistryError>>()?;

        let mut workspace_statement = self.connection.prepare(
            "SELECT workspace_uid, project_uid, is_project_home, state, last_seen_at, \
                    created_at, updated_at \
             FROM workspace_registry WHERE locator = ?1",
        )?;
        let workspaces = workspace_statement
            .query_map(params![text], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(
                |(
                    workspace_uid,
                    project_uid,
                    is_project_home,
                    state_raw,
                    last_seen_at,
                    created_at,
                    updated_at,
                )| {
                    Ok(WorkspaceRegistryEntry {
                        workspace_id: workspace_id_from_blob(workspace_uid),
                        project_id: project_id_from_blob(project_uid),
                        locator: locator.to_path_buf(),
                        is_project_home,
                        state: RegistryState::parse(&state_raw)?,
                        last_seen_at,
                        created_at,
                        updated_at,
                    })
                },
            )
            .collect::<Result<Vec<_>, RegistryError>>()?;

        Ok(LocatorCandidates {
            projects,
            workspaces,
        })
    }

    /// Record that `git_common_dir` (Git's canonical common-git-directory
    /// plumbing path; see #15 task 7) belongs to `project_id`'s lineage, or
    /// confirm an unchanged re-registration.
    ///
    /// Create-only, mirroring [`Self::register_project`]: the same common
    /// Git directory observed for two different Projects is a conflict,
    /// never a silent merge.
    pub fn register_git_lineage(
        &self,
        git_common_dir: &Path,
        project_id: ProjectId,
    ) -> Result<(), RegistryError> {
        if let Some(existing) = self.find_project_by_git_lineage(git_common_dir)? {
            if existing == project_id {
                return Ok(());
            }
            return Err(RegistryError::GitLineageConflict {
                git_common_dir: git_common_dir.to_path_buf(),
                existing_project_id: existing,
                requested_project_id: project_id,
            });
        }

        let now = db::now_millis_text();
        self.connection.execute(
            "INSERT INTO project_git_lineage (git_common_dir, project_uid, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?3)",
            params![
                locator_to_text(git_common_dir),
                project_id.to_bytes().to_vec(),
                now
            ],
        )?;
        Ok(())
    }

    /// Look up the Project (if any) whose lineage claims `git_common_dir`.
    pub fn find_project_by_git_lineage(
        &self,
        git_common_dir: &Path,
    ) -> Result<Option<ProjectId>, RegistryError> {
        self.connection
            .query_row(
                "SELECT project_uid FROM project_git_lineage WHERE git_common_dir = ?1",
                params![locator_to_text(git_common_dir)],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map(|found| found.map(project_id_from_blob))
            .map_err(RegistryError::from)
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

        // Idempotent means "no new row and the same identity/locator" --
        // not "no observable change at all". #15 task 11: a matching
        // re-registration is itself an observation, so it refreshes
        // state/last_seen_at/updated_at.
        assert_eq!(first.project_id, second.project_id);
        assert_eq!(first.home_locator, second.home_locator);
        assert_eq!(first.created_at, second.created_at);
        assert_eq!(second.state, RegistryState::Active);
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

    #[test]
    fn git_lineage_round_trips_and_is_idempotent() {
        let dir = TestDir::create("git-lineage-roundtrip");
        let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");
        let project_id = ProjectId::generate();
        registry
            .register_project(project_id, Path::new("/repo/main"))
            .expect("project should register");
        let common_dir = Path::new("/repo/main/.git");

        registry
            .register_git_lineage(common_dir, project_id)
            .expect("first lineage registration should succeed");
        registry
            .register_git_lineage(common_dir, project_id)
            .expect("re-registering the same lineage should be idempotent");

        let found = registry
            .find_project_by_git_lineage(common_dir)
            .expect("lookup should succeed");
        assert_eq!(found, Some(project_id));
    }

    #[test]
    fn unregistered_git_lineage_is_none() {
        let dir = TestDir::create("git-lineage-missing");
        let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");

        let found = registry
            .find_project_by_git_lineage(Path::new("/repo/unknown/.git"))
            .expect("lookup should succeed");
        assert_eq!(found, None);
    }

    #[test]
    fn git_lineage_conflict_is_rejected() {
        let dir = TestDir::create("git-lineage-conflict");
        let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");
        let first_project = ProjectId::generate();
        let second_project = ProjectId::generate();
        registry
            .register_project(first_project, Path::new("/repo/first"))
            .expect("first project should register");
        registry
            .register_project(second_project, Path::new("/repo/second"))
            .expect("second project should register");
        let common_dir = Path::new("/repo/first/.git");
        registry
            .register_git_lineage(common_dir, first_project)
            .expect("first lineage registration should succeed");

        let error = registry
            .register_git_lineage(common_dir, second_project)
            .expect_err("the same common Git directory must never be linked to two Projects");

        assert!(matches!(
            error,
            RegistryError::GitLineageConflict { existing_project_id, .. }
            if existing_project_id == first_project
        ));
    }

    #[test]
    fn fresh_registrations_start_active_with_last_seen_set() {
        let dir = TestDir::create("fresh-active");
        let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");
        let project_id = ProjectId::generate();
        let workspace_id = WorkspaceId::generate();

        let project = registry
            .register_project(project_id, Path::new("/repo/main"))
            .expect("project should register");
        assert_eq!(project.state, RegistryState::Active);
        assert!(project.last_seen_at.is_some());

        let workspace = registry
            .register_workspace(workspace_id, project_id, Path::new("/repo/main"), true)
            .expect("workspace should register");
        assert_eq!(workspace.state, RegistryState::Active);
        assert!(workspace.last_seen_at.is_some());
    }

    #[test]
    fn project_missing_then_active_updates_state_and_preserves_then_refreshes_last_seen() {
        let dir = TestDir::create("project-missing-active");
        let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");
        let project_id = ProjectId::generate();
        let created = registry
            .register_project(project_id, Path::new("/repo/main"))
            .expect("project should register");
        let seen_when_created = created.last_seen_at.clone();

        let missing = registry
            .mark_project_missing(project_id)
            .expect("marking missing should succeed");
        assert_eq!(missing.state, RegistryState::Missing);
        // last_seen_at records the last time it was genuinely seen, so a
        // negative observation must not touch it.
        assert_eq!(missing.last_seen_at, seen_when_created);

        let active = registry
            .mark_project_active(project_id)
            .expect("marking active should succeed");
        assert_eq!(active.state, RegistryState::Active);
        assert!(active.last_seen_at.is_some());
    }

    #[test]
    fn workspace_missing_then_active_updates_state() {
        let dir = TestDir::create("workspace-missing-active");
        let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");
        let project_id = ProjectId::generate();
        let workspace_id = WorkspaceId::generate();
        registry
            .register_project(project_id, Path::new("/repo/main"))
            .expect("project should register");
        registry
            .register_workspace(workspace_id, project_id, Path::new("/repo/main"), true)
            .expect("workspace should register");

        let missing = registry
            .mark_workspace_missing(workspace_id)
            .expect("marking missing should succeed");
        assert_eq!(missing.state, RegistryState::Missing);

        let active = registry
            .mark_workspace_active(workspace_id)
            .expect("marking active should succeed");
        assert_eq!(active.state, RegistryState::Active);
    }

    #[test]
    fn reregistering_a_missing_project_marks_it_active_again() {
        let dir = TestDir::create("rediscover-project");
        let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");
        let project_id = ProjectId::generate();
        registry
            .register_project(project_id, Path::new("/repo/main"))
            .expect("project should register");
        registry
            .mark_project_missing(project_id)
            .expect("marking missing should succeed");

        let rediscovered = registry
            .register_project(project_id, Path::new("/repo/main"))
            .expect("re-registering the same locator should succeed");
        assert_eq!(rediscovered.state, RegistryState::Active);
    }

    #[test]
    fn reregistering_a_missing_workspace_marks_it_active_again() {
        let dir = TestDir::create("rediscover-workspace");
        let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");
        let project_id = ProjectId::generate();
        let workspace_id = WorkspaceId::generate();
        registry
            .register_project(project_id, Path::new("/repo/main"))
            .expect("project should register");
        registry
            .register_workspace(workspace_id, project_id, Path::new("/repo/main"), true)
            .expect("workspace should register");
        registry
            .mark_workspace_missing(workspace_id)
            .expect("marking missing should succeed");

        let rediscovered = registry
            .register_workspace(workspace_id, project_id, Path::new("/repo/main"), true)
            .expect("re-registering the same locator should succeed");
        assert_eq!(rediscovered.state, RegistryState::Active);
    }

    #[test]
    fn locator_update_marks_the_entry_active() {
        let dir = TestDir::create("locator-update-active");
        let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");
        let project_id = ProjectId::generate();
        let workspace_id = WorkspaceId::generate();
        registry
            .register_project(project_id, Path::new("/repo/main"))
            .expect("project should register");
        registry
            .register_workspace(workspace_id, project_id, Path::new("/repo/main"), true)
            .expect("workspace should register");
        registry
            .mark_project_missing(project_id)
            .expect("marking missing should succeed");
        registry
            .mark_workspace_missing(workspace_id)
            .expect("marking missing should succeed");

        let moved_project = registry
            .update_project_locator(project_id, Path::new("/repo/moved"))
            .expect("project relocation should succeed");
        assert_eq!(moved_project.state, RegistryState::Active);

        let moved_workspace = registry
            .update_workspace_locator(workspace_id, Path::new("/repo/moved"))
            .expect("workspace relocation should succeed");
        assert_eq!(moved_workspace.state, RegistryState::Active);
    }

    #[test]
    fn marking_an_unknown_project_or_workspace_is_rejected() {
        let dir = TestDir::create("mark-unknown");
        let registry = GlobalRegistry::open(&dir.db_path()).expect("registry should open");
        let unknown_project = ProjectId::generate();
        let unknown_workspace = WorkspaceId::generate();

        assert!(matches!(
            registry.mark_project_missing(unknown_project),
            Err(RegistryError::UnknownProject { project_id }) if project_id == unknown_project
        ));
        assert!(matches!(
            registry.mark_project_active(unknown_project),
            Err(RegistryError::UnknownProject { project_id }) if project_id == unknown_project
        ));
        assert!(matches!(
            registry.mark_workspace_missing(unknown_workspace),
            Err(RegistryError::UnknownWorkspace { workspace_id }) if workspace_id == unknown_workspace
        ));
        assert!(matches!(
            registry.mark_workspace_active(unknown_workspace),
            Err(RegistryError::UnknownWorkspace { workspace_id }) if workspace_id == unknown_workspace
        ));
    }
}
