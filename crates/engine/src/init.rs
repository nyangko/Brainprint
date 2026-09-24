//! Workspace init orchestration: fresh Project-home and secondary Git
//! worktree, Git and non-Git (#15 task 6/7 / #13 task 3 §7, task 5 §4,
//! task 5 §17, task 9 §2-3).
//!
//! This connects tasks 1-5 (identity types, path/config bootstrap, the
//! migration runner, the global registry, and project/workspace/index
//! schema) into one orchestration covering two cases:
//! - **fresh Project-home**: a directory Brainprint has never touched
//!   before, Git or not, with no evidence of belonging to an already
//!   registered Project. This Workspace becomes its Project's canonical
//!   `project.db` owner.
//! - **secondary Git worktree**: a Git worktree whose common Git directory
//!   (Git's own on-disk plumbing evidence, not a path/branch-name guess —
//!   #15 task 7 rule 8-9) is already linked to a registered Project. This
//!   Workspace gets a new WorkspaceID under the *same* ProjectID, its own
//!   `workspace.db`/`index.db`, and never a local `project.db` copy — it
//!   shares the existing project-home's store via the global registry.
//!
//! Routing between the two is evidence-based, not a guess: a worktree with
//! insufficient or unrelated Git evidence (no `commondir`, or a common
//! directory the registry has never seen) is treated as an independent
//! fresh case rather than force-linked to an unrelated Project (#15 task 7
//! rule 11-12: prefer false split/conflict over false merge).
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
    generation::{GenerationError, GenerationStore},
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
    /// A secondary worktree resolved to a registered Project whose
    /// project-home `project.db` is missing (or unbound) at its registry
    /// locator. #15 task 7 forbids silently creating a fresh `project.db`
    /// here or elsewhere — this is an explicit missing/recovery-required
    /// result (actual recovery is #15 task 11).
    ProjectHomeMissing {
        project_id: ProjectId,
        home_locator: PathBuf,
    },
    /// Reconciling orphaned `BUILDING` generations (#15 task 11) failed.
    Generation(GenerationError),
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
            Self::ProjectHomeMissing {
                project_id,
                home_locator,
            } => write!(
                formatter,
                "project {project_id}'s project-home store is missing at {} \
                 (registry locator); explicit recovery is required (#15 task 11)",
                home_locator.display()
            ),
            Self::Generation(source) => write!(formatter, "{source}"),
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
            Self::Generation(source) => Some(source),
            Self::MissingParent { .. }
            | Self::UnsupportedIdentityFormat { .. }
            | Self::ProjectIdentityMismatch { .. }
            | Self::WorkspaceIdentityMismatch { .. }
            | Self::CorruptDbMeta { .. }
            | Self::ProjectHomeMissing { .. } => None,
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

impl From<GenerationError> for InitError {
    fn from(source: GenerationError) -> Self {
        Self::Generation(source)
    }
}

/// Init `requested_root` as a Brainprint Workspace, or verify/resume one
/// already initialized there.
///
/// For a Git repository or worktree, the Workspace root is the nearest
/// ancestor containing `.git` (not necessarily `requested_root` itself);
/// for a non-Git directory, `requested_root` itself is the Workspace root.
/// `global_paths` is the already-resolved `~/.brainprint` location (see
/// [`GlobalPaths::discover`] / [`GlobalPaths::from_home`]).
///
/// Routing (#15 task 7):
/// 1. A Workspace that already has a local identity (`workspace.toml`)
///    resumes with that identity, and is a project-home or secondary
///    worktree according to what the registry already recorded for its
///    Project (never re-derived from Git evidence at resume time).
/// 2. Otherwise, for a Git Workspace, its common Git directory (Git's own
///    on-disk plumbing, see [`git_common_dir`]) is looked up in the
///    registry's lineage table. A hit means this is a secondary worktree of
///    an already-registered Project: same ProjectID, a new WorkspaceID, and
///    no local `project.db`.
/// 3. Anything else (non-Git, or Git with no linkable lineage evidence) is
///    a fresh Project-home init, exactly as #15 task 6.
pub fn init_workspace(
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

    let registry = GlobalRegistry::open(&global_paths.global_db)?;

    if let Some(existing) = read_identity_file(&workspace_paths.identity_file)? {
        // #15 task 7/11 rule: resuming a Workspace never re-derives its
        // role from Git evidence -- the registry's durable
        // `is_project_home` flag for *this WorkspaceID* is the single
        // source of truth for "is this the project-home or a secondary
        // worktree". This must not be re-derived by comparing the
        // project's home_locator to the current workspace_root: once the
        // project-home directory itself moves (#15 task 11), that
        // comparison would wrongly stop matching and misroute a
        // project-home reopen as a secondary worktree.
        let is_project_home = registry
            .get_workspace(existing.workspace_id)?
            .map(|entry| entry.is_project_home)
            // No registry entry yet only happens mid-way through a
            // partially-failed fresh project-home init (the registry write
            // comes last); resuming that completes it as project-home.
            .unwrap_or(true);

        return if is_project_home {
            finish_project_home(
                existing,
                false,
                workspace_root,
                is_git,
                &workspace_paths,
                &registry,
            )
        } else {
            finish_secondary_worktree(existing, false, workspace_root, &workspace_paths, &registry)
        };
    }

    if is_git
        && let Some(common_dir) = git_common_dir(&workspace_root)?
        && let Some(project_id) = registry.find_project_by_git_lineage(&common_dir)?
    {
        let identity = WorkspaceIdentity {
            format_version: IDENTITY_FORMAT_VERSION,
            project_id,
            workspace_id: WorkspaceId::generate(),
            created_at: db::now_millis_text(),
        };
        write_identity_file(&workspace_paths.identity_file, &identity)?;
        return finish_secondary_worktree(
            identity,
            true,
            workspace_root,
            &workspace_paths,
            &registry,
        );
    }

    let identity = WorkspaceIdentity {
        format_version: IDENTITY_FORMAT_VERSION,
        project_id: ProjectId::generate(),
        workspace_id: WorkspaceId::generate(),
        created_at: db::now_millis_text(),
    };
    write_identity_file(&workspace_paths.identity_file, &identity)?;
    finish_project_home(
        identity,
        true,
        workspace_root,
        is_git,
        &workspace_paths,
        &registry,
    )
}

/// Complete/verify a project-home Workspace: the one Workspace that owns
/// its Project's canonical `project.db` (#13 task 5 §4).
fn finish_project_home(
    identity: WorkspaceIdentity,
    freshly_created: bool,
    workspace_root: PathBuf,
    is_git: bool,
    workspace_paths: &WorkspacePaths,
    registry: &GlobalRegistry,
) -> Result<InitOutcome, InitError> {
    if !freshly_created && !workspace_paths.project_db.is_file() {
        // #15 task 11: resuming an *existing* project-home identity whose
        // project.db has disappeared must never be papered over by
        // `schema::project::open` silently creating a fresh empty one --
        // that would be exactly the durable-knowledge loss task 7/11
        // forbid. A first-ever init (`freshly_created`) is the only case
        // where project.db legitimately does not exist yet.
        //
        // But only when the registry *agrees* this locator is the
        // project-home: if it instead points elsewhere, this is a
        // workspace.toml copied to a second location without its DBs
        // (the real project.db is intact at the registered locator) --
        // a duplicate/conflict, not a missing store. Falling through lets
        // the normal registration path below reject it as a conflict
        // (false split over false merge), instead of misreporting it as
        // "missing".
        let registered_here = registry
            .get_project(identity.project_id)?
            .map(|entry| entry.home_locator == workspace_root)
            .unwrap_or(true);
        if registered_here {
            let _ = registry.mark_project_missing(identity.project_id);
            return Err(InitError::ProjectHomeMissing {
                project_id: identity.project_id,
                home_locator: workspace_root,
            });
        }
        // Registry disagrees this is the current home locator -- probe
        // the conflict *before* creating any local project.db here, so a
        // duplicate/copy is rejected without leaving a stray empty DB
        // behind. `register_project_with_move_repair` is idempotent, so
        // calling it again later in the normal flow below is harmless.
        register_project_with_move_repair(registry, identity.project_id, &workspace_root)?;
    }

    config::bootstrap_workspace_config(workspace_paths)?;

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
    // #15 task 11 / #4 task 5 §8: a BUILDING generation left over from a
    // process that never reopened this index.db again is never promoted
    // to STABLE just because it's the only thing here -- abort it
    // explicitly. Never touches the existing stable pointer.
    GenerationStore::from_connection(index_db.connection).reconcile_orphan_generations()?;

    register_project_with_move_repair(registry, identity.project_id, &workspace_root)?;
    register_workspace_with_move_repair(
        registry,
        identity.workspace_id,
        identity.project_id,
        &workspace_root,
        true,
    )?;

    if is_git && let Some(common_dir) = git_common_dir(&workspace_root)? {
        registry.register_git_lineage(&common_dir, identity.project_id)?;
    }

    Ok(InitOutcome {
        project_id: identity.project_id,
        workspace_id: identity.workspace_id,
        workspace_root,
        is_git,
        freshly_created,
    })
}

/// Complete/verify a secondary Git worktree Workspace: same ProjectID as
/// its project-home, its own `workspace.db`/`index.db`, and never a local
/// `project.db` (#15 task 7).
fn finish_secondary_worktree(
    identity: WorkspaceIdentity,
    freshly_created: bool,
    workspace_root: PathBuf,
    workspace_paths: &WorkspacePaths,
    registry: &GlobalRegistry,
) -> Result<InitOutcome, InitError> {
    verify_project_home_store(registry, identity.project_id)?;

    config::bootstrap_workspace_config(workspace_paths)?;

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
    GenerationStore::from_connection(index_db.connection).reconcile_orphan_generations()?;

    register_workspace_with_move_repair(
        registry,
        identity.workspace_id,
        identity.project_id,
        &workspace_root,
        false,
    )?;

    Ok(InitOutcome {
        project_id: identity.project_id,
        workspace_id: identity.workspace_id,
        workspace_root,
        is_git: true,
        freshly_created,
    })
}

/// [`GlobalRegistry::register_project`] as-is, unless it hits a
/// [`RegistryError::ProjectLocatorConflict`] whose *old* locator no longer
/// exists on disk at all (#15 task 11): that is strong, direct evidence of
/// a plain directory move rather than a copy/duplicate, so the identity is
/// kept and the registry's locator is repaired instead of rejected. Any
/// other conflict (the old locator still exists -- ambiguous, possibly a
/// live duplicate) is rejected as before: false split/conflict over false
/// merge.
fn register_project_with_move_repair(
    registry: &GlobalRegistry,
    project_id: ProjectId,
    new_locator: &Path,
) -> Result<(), InitError> {
    match registry.register_project(project_id, new_locator) {
        Ok(_) => Ok(()),
        Err(RegistryError::ProjectLocatorConflict {
            existing_locator, ..
        }) if !existing_locator.exists() => {
            registry.update_project_locator(project_id, new_locator)?;
            Ok(())
        }
        Err(other) => Err(other.into()),
    }
}

/// [`GlobalRegistry::register_workspace`]'s counterpart to
/// [`register_project_with_move_repair`]: only repairs when the conflict
/// is against the *same* Project (this Workspace, not a different one
/// colliding on the same stable ID) and the old locator is confirmed gone.
fn register_workspace_with_move_repair(
    registry: &GlobalRegistry,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    new_locator: &Path,
    is_project_home: bool,
) -> Result<(), InitError> {
    match registry.register_workspace(workspace_id, project_id, new_locator, is_project_home) {
        Ok(_) => Ok(()),
        Err(RegistryError::WorkspaceIdentityConflict {
            existing_project_id,
            existing_locator,
            ..
        }) if existing_project_id == project_id && !existing_locator.exists() => {
            registry.update_workspace_locator(workspace_id, new_locator)?;
            Ok(())
        }
        Err(other) => Err(other.into()),
    }
}

/// Confirm `project_id`'s project-home `project.db` actually exists and is
/// bound to `project_id`, without ever creating it (#15 task 7: a missing
/// project-home store is an explicit missing/recovery-required result, not
/// something to paper over with a fresh empty DB). On success, marks the
/// Project [`crate::registry::RegistryState::Active`]; on a missing store,
/// marks it [`crate::registry::RegistryState::Missing`] (#15 task 11) so
/// the registry reflects what reopen actually observed.
fn verify_project_home_store(
    registry: &GlobalRegistry,
    project_id: ProjectId,
) -> Result<(), InitError> {
    let project_entry = registry
        .get_project(project_id)?
        .ok_or(RegistryError::UnknownProject { project_id })?;

    let home_paths = WorkspacePaths::from_root(&project_entry.home_locator);
    if !home_paths.project_db.is_file() {
        let _ = registry.mark_project_missing(project_id);
        return Err(InitError::ProjectHomeMissing {
            project_id,
            home_locator: project_entry.home_locator,
        });
    }

    let project_db = schema::project::open(&home_paths.project_db)?;
    let bound: Option<Vec<u8>> = project_db.connection.query_row(
        "SELECT project_uid FROM db_meta WHERE id = 0",
        [],
        |row| row.get(0),
    )?;

    match bound {
        Some(bytes) => {
            let found = stable_id_from_blob::<ProjectId>(&bytes, DbKind::Project)?;
            if found == project_id {
                registry.mark_project_active(project_id)?;
                Ok(())
            } else {
                Err(InitError::ProjectIdentityMismatch {
                    expected: project_id,
                    found,
                })
            }
        }
        None => {
            let _ = registry.mark_project_missing(project_id);
            Err(InitError::ProjectHomeMissing {
                project_id,
                home_locator: project_entry.home_locator,
            })
        }
    }
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

/// Resolve the canonical Git "common directory" for `workspace_root`'s
/// `.git` entry by following Git's own on-disk plumbing files -- never by
/// invoking `git` or guessing from paths/branch names (#15 task 7 rule
/// 8-9).
///
/// - `.git` as a directory: the common directory is that directory itself.
/// - `.git` as a file (`gitdir: <path>`, e.g. a linked worktree or a
///   submodule): follows it to the private git dir, then reads that dir's
///   `commondir` file (present only for a linked worktree) to find the
///   shared common directory.
///
/// Returns `Ok(None)` whenever the evidence is incomplete or unresolvable
/// (missing `.git`, a `gitdir` pointer to nowhere, or a private git dir
/// with no `commondir` -- e.g. a submodule, which is not a linked worktree
/// of anything) -- insufficient evidence to claim a lineage connection, so
/// callers fall back to treating the target as independent (task 7 rule
/// 12: prefer false split over false merge).
fn git_common_dir(workspace_root: &Path) -> Result<Option<PathBuf>, InitError> {
    let dot_git = workspace_root.join(".git");
    let metadata = match fs::symlink_metadata(&dot_git) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(InitError::Io {
                path: dot_git,
                source,
            });
        }
    };

    if metadata.is_dir() {
        return canonicalize_lenient(&dot_git);
    }

    let contents = fs::read_to_string(&dot_git).map_err(|source| InitError::Io {
        path: dot_git.clone(),
        source,
    })?;
    let Some(target) = contents.strip_prefix("gitdir:") else {
        return Ok(None);
    };
    let Some(private_git_dir) =
        canonicalize_lenient(&resolve_relative(workspace_root, target.trim()))?
    else {
        return Ok(None);
    };

    let commondir_file = private_git_dir.join("commondir");
    let Ok(commondir_contents) = fs::read_to_string(&commondir_file) else {
        return Ok(None);
    };
    canonicalize_lenient(&resolve_relative(
        &private_git_dir,
        commondir_contents.trim(),
    ))
}

fn resolve_relative(base_dir: &Path, text: &str) -> PathBuf {
    let candidate = Path::new(text);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base_dir.join(candidate)
    }
}

fn canonicalize_lenient(path: &Path) -> Result<Option<PathBuf>, InitError> {
    match fs::canonicalize(path) {
        Ok(canonical) => Ok(Some(canonical)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(InitError::Io {
            path: path.to_path_buf(),
            source,
        }),
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

    /// Run a real `git` command for worktree lineage tests. Unlike
    /// [`make_git_repo`]'s bare `.git` directory fixture, `git worktree
    /// add` needs an actual repository so the `commondir` plumbing this
    /// module reads is genuinely present.
    fn run_git(args: &[&str], cwd: &Path) {
        let output = process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "brainprint-test")
            .env("GIT_AUTHOR_EMAIL", "test@brainprint.invalid")
            .env("GIT_COMMITTER_NAME", "brainprint-test")
            .env("GIT_COMMITTER_EMAIL", "test@brainprint.invalid")
            .output()
            .expect("git should be installed and runnable for this test");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn real_git_repo(workspace: &TestDir) {
        run_git(&["init", "-q"], workspace.path());
        run_git(
            &["commit", "-q", "--allow-empty", "-m", "init"],
            workspace.path(),
        );
    }

    fn add_worktree(main_repo: &TestDir, worktree: &TestDir, branch: &str) {
        run_git(
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                &worktree.path().to_string_lossy(),
            ],
            main_repo.path(),
        );
    }

    #[test]
    fn fresh_git_repository_init_succeeds() {
        let global_home = TestDir::create("git-fresh-global");
        let workspace = TestDir::create("git-fresh-workspace");
        make_git_repo(&workspace);

        let outcome = init_workspace(workspace.path(), &global_paths(&global_home))
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

        let outcome = init_workspace(workspace.path(), &paths).expect("init should succeed");

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

        let outcome = init_workspace(workspace.path(), &paths).expect("init should succeed");

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

        let outcome = init_workspace(workspace.path(), &paths).expect("init should succeed");
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

        let outcome = init_workspace(workspace.path(), &paths).expect("init should succeed");
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
            init_workspace(workspace.path(), &paths).expect("non-git init should succeed");

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

        let outcome = init_workspace(workspace.path(), &paths).expect("init should succeed");
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

        let outcome = init_workspace(&nested, &paths).expect("init from nested dir should succeed");
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

        let first = init_workspace(workspace.path(), &paths).expect("first init should succeed");
        assert!(first.freshly_created);

        let second = init_workspace(workspace.path(), &paths).expect("second init should succeed");
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

        let error = init_workspace(workspace.path(), &paths)
            .expect_err("malformed identity must be rejected");
        assert!(matches!(error, InitError::MalformedIdentity { .. }));
    }

    #[test]
    fn project_db_identity_mismatch_is_rejected() {
        let global_home = TestDir::create("project-mismatch-global");
        let workspace = TestDir::create("project-mismatch-workspace");
        make_git_repo(&workspace);
        let paths = global_paths(&global_home);

        let outcome = init_workspace(workspace.path(), &paths).expect("first init should succeed");
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

        let error = init_workspace(workspace.path(), &paths)
            .expect_err("mismatched project.db identity must be rejected");
        assert!(matches!(error, InitError::ProjectIdentityMismatch { .. }));
    }

    #[test]
    fn workspace_db_identity_mismatch_is_rejected() {
        let global_home = TestDir::create("workspace-mismatch-global");
        let workspace = TestDir::create("workspace-mismatch-workspace");
        make_git_repo(&workspace);
        let paths = global_paths(&global_home);

        let outcome = init_workspace(workspace.path(), &paths).expect("first init should succeed");
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

        let error = init_workspace(workspace.path(), &paths)
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
            init_workspace(original.path(), &paths).expect("original init should succeed");

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

        let error = init_workspace(copy.path(), &paths)
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

        let error =
            init_workspace(workspace.path(), &paths).expect_err("blocked data dir must fail init");
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

        init_workspace(workspace.path(), &paths).expect("init should succeed");

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

        let outcome =
            init_workspace(workspace.path(), &paths).expect("worktree-style init should succeed");
        assert!(outcome.is_git);
        assert!(outcome.freshly_created);
    }

    #[test]
    fn secondary_worktree_shares_project_id_and_gets_new_workspace_id() {
        let global_home = TestDir::create("worktree-basic-global");
        let main_repo = TestDir::create("worktree-basic-main");
        real_git_repo(&main_repo);
        let paths = global_paths(&global_home);

        let main_outcome =
            init_workspace(main_repo.path(), &paths).expect("main init should succeed");
        assert!(main_outcome.is_git);

        let secondary = TestDir::create("worktree-basic-secondary");
        add_worktree(&main_repo, &secondary, "feature-x");

        let secondary_outcome =
            init_workspace(secondary.path(), &paths).expect("secondary init should succeed");

        assert_eq!(secondary_outcome.project_id, main_outcome.project_id);
        assert_ne!(secondary_outcome.workspace_id, main_outcome.workspace_id);

        let main_paths = WorkspacePaths::from_root(&main_outcome.workspace_root);
        assert!(main_paths.project_db.is_file());

        let secondary_paths = WorkspacePaths::from_root(&secondary_outcome.workspace_root);
        assert!(!secondary_paths.project_db.exists());
        assert!(secondary_paths.workspace_db.is_file());
        assert!(secondary_paths.index_db.is_file());

        let registry = GlobalRegistry::open(&paths.global_db).expect("registry should open");
        let project = registry
            .get_project(main_outcome.project_id)
            .expect("lookup should succeed")
            .expect("project should be registered");
        assert_eq!(project.home_locator, main_outcome.workspace_root);

        let main_entry = registry
            .get_workspace(main_outcome.workspace_id)
            .expect("lookup should succeed")
            .expect("main workspace should be registered");
        assert!(main_entry.is_project_home);

        let secondary_entry = registry
            .get_workspace(secondary_outcome.workspace_id)
            .expect("lookup should succeed")
            .expect("secondary workspace should be registered");
        assert!(!secondary_entry.is_project_home);
        assert_eq!(secondary_entry.project_id, main_outcome.project_id);
    }

    #[test]
    fn multiple_secondary_worktrees_share_project_id_with_unique_workspace_ids() {
        let global_home = TestDir::create("worktree-multi-global");
        let main_repo = TestDir::create("worktree-multi-main");
        real_git_repo(&main_repo);
        let paths = global_paths(&global_home);

        let main_outcome =
            init_workspace(main_repo.path(), &paths).expect("main init should succeed");

        let worktree_a = TestDir::create("worktree-multi-a");
        add_worktree(&main_repo, &worktree_a, "feature-a");
        let outcome_a =
            init_workspace(worktree_a.path(), &paths).expect("worktree a init should succeed");

        let worktree_b = TestDir::create("worktree-multi-b");
        add_worktree(&main_repo, &worktree_b, "feature-b");
        let outcome_b =
            init_workspace(worktree_b.path(), &paths).expect("worktree b init should succeed");

        assert_eq!(outcome_a.project_id, main_outcome.project_id);
        assert_eq!(outcome_b.project_id, main_outcome.project_id);

        let mut workspace_ids = vec![
            main_outcome.workspace_id,
            outcome_a.workspace_id,
            outcome_b.workspace_id,
        ];
        workspace_ids.sort();
        workspace_ids.dedup();
        assert_eq!(
            workspace_ids.len(),
            3,
            "all three Workspaces must have distinct WorkspaceIDs"
        );
    }

    #[test]
    fn secondary_worktree_reinit_is_idempotent_and_creates_no_project_db() {
        let global_home = TestDir::create("worktree-reinit-global");
        let main_repo = TestDir::create("worktree-reinit-main");
        real_git_repo(&main_repo);
        let paths = global_paths(&global_home);
        init_workspace(main_repo.path(), &paths).expect("main init should succeed");

        let secondary = TestDir::create("worktree-reinit-secondary");
        add_worktree(&main_repo, &secondary, "feature-reinit");

        let first =
            init_workspace(secondary.path(), &paths).expect("first secondary init should succeed");
        assert!(first.freshly_created);

        let second =
            init_workspace(secondary.path(), &paths).expect("second secondary init should succeed");
        assert!(!second.freshly_created);
        assert_eq!(second.project_id, first.project_id);
        assert_eq!(second.workspace_id, first.workspace_id);

        let secondary_paths = WorkspacePaths::from_root(&second.workspace_root);
        assert!(!secondary_paths.project_db.exists());
    }

    #[test]
    fn copied_secondary_workspace_toml_is_rejected_as_duplicate_workspace_id() {
        let global_home = TestDir::create("worktree-copy-global");
        let main_repo = TestDir::create("worktree-copy-main");
        real_git_repo(&main_repo);
        let paths = global_paths(&global_home);
        init_workspace(main_repo.path(), &paths).expect("main init should succeed");

        let original_secondary = TestDir::create("worktree-copy-original");
        add_worktree(&main_repo, &original_secondary, "feature-copy-original");
        let original_outcome = init_workspace(original_secondary.path(), &paths)
            .expect("original secondary init should succeed");

        // Simulate copying `.brainprint/workspace.toml` into a second,
        // independent worktree: same WorkspaceID, different locator.
        let copy_target = TestDir::create("worktree-copy-target");
        add_worktree(&main_repo, &copy_target, "feature-copy-target");
        let copy_canonical =
            fs::canonicalize(copy_target.path()).expect("copy path should canonicalize");
        let copy_workspace_paths = WorkspacePaths::from_root(&copy_canonical);
        let original_workspace_paths = WorkspacePaths::from_root(&original_outcome.workspace_root);
        fs::create_dir_all(&copy_workspace_paths.root).expect("root should be created");
        fs::copy(
            &original_workspace_paths.identity_file,
            &copy_workspace_paths.identity_file,
        )
        .expect("identity file should copy");

        let error = init_workspace(copy_target.path(), &paths)
            .expect_err("the same WorkspaceID at a second locator must be rejected");
        assert!(matches!(error, InitError::Registry(_)));
    }

    #[test]
    fn missing_project_home_store_is_rejected_not_silently_recreated() {
        let global_home = TestDir::create("worktree-missing-home-global");
        let main_repo = TestDir::create("worktree-missing-home-main");
        real_git_repo(&main_repo);
        let paths = global_paths(&global_home);
        let main_outcome =
            init_workspace(main_repo.path(), &paths).expect("main init should succeed");

        let secondary = TestDir::create("worktree-missing-home-secondary");
        add_worktree(&main_repo, &secondary, "feature-missing-home");

        let main_paths = WorkspacePaths::from_root(&main_outcome.workspace_root);
        fs::remove_file(&main_paths.project_db).expect("project.db fixture removal should succeed");

        let error = init_workspace(secondary.path(), &paths)
            .expect_err("a missing project-home store must be rejected, not silently recreated");
        assert!(matches!(error, InitError::ProjectHomeMissing { .. }));
        assert!(
            !main_paths.project_db.exists(),
            "a missing project-home store must never be silently recreated"
        );
    }

    #[test]
    fn unrelated_fresh_git_repository_still_gets_its_own_project_id() {
        let global_home = TestDir::create("worktree-unrelated-global");
        let paths = global_paths(&global_home);

        let first_repo = TestDir::create("worktree-unrelated-first");
        real_git_repo(&first_repo);
        let first_outcome =
            init_workspace(first_repo.path(), &paths).expect("first repo init should succeed");

        let second_repo = TestDir::create("worktree-unrelated-second");
        real_git_repo(&second_repo);
        let second_outcome =
            init_workspace(second_repo.path(), &paths).expect("second repo init should succeed");

        assert_ne!(first_outcome.project_id, second_outcome.project_id);
        assert!(first_outcome.freshly_created);
        assert!(second_outcome.freshly_created);
    }

    // --- #15 task 11: restart/reopen identity/DB mismatch recovery ---

    use crate::registry::RegistryState;

    #[test]
    fn restart_reidentifies_project_home_and_secondary_worktree_without_new_ids() {
        let global_home = TestDir::create("restart-global");
        let paths = global_paths(&global_home);
        let main_repo = TestDir::create("restart-main");
        real_git_repo(&main_repo);

        let main_before =
            init_workspace(main_repo.path(), &paths).expect("main init should succeed");

        let secondary = TestDir::create("restart-secondary");
        add_worktree(&main_repo, &secondary, "feature-restart");
        let secondary_before =
            init_workspace(secondary.path(), &paths).expect("secondary init should succeed");

        // Simulate a daemon restart: every value below is freshly derived
        // from what init_workspace reads back off disk (workspace.toml,
        // global.db, project.db/workspace.db/index.db) -- nothing
        // in-process is reused across these two calls.
        let main_after = init_workspace(main_repo.path(), &paths)
            .expect("main reopen after restart should succeed");
        let secondary_after = init_workspace(secondary.path(), &paths)
            .expect("secondary reopen after restart should succeed");

        assert_eq!(main_after.project_id, main_before.project_id);
        assert_eq!(main_after.workspace_id, main_before.workspace_id);
        assert!(!main_after.freshly_created);

        assert_eq!(secondary_after.project_id, secondary_before.project_id);
        assert_eq!(secondary_after.workspace_id, secondary_before.workspace_id);
        assert_eq!(secondary_after.project_id, main_before.project_id);
        assert!(!secondary_after.freshly_created);
    }

    #[test]
    fn project_home_resume_with_missing_project_db_is_rejected_not_recreated() {
        let global_home = TestDir::create("home-missing-global");
        let workspace = TestDir::create("home-missing-workspace");
        make_git_repo(&workspace);
        let paths = global_paths(&global_home);
        let outcome =
            init_workspace(workspace.path(), &paths).expect("initial init should succeed");

        let workspace_paths = WorkspacePaths::from_root(&outcome.workspace_root);
        fs::remove_file(&workspace_paths.project_db)
            .expect("project.db fixture removal should succeed");

        let error = init_workspace(workspace.path(), &paths)
            .expect_err("resuming with a missing project.db must be rejected");
        assert!(matches!(error, InitError::ProjectHomeMissing { .. }));
        assert!(
            !workspace_paths.project_db.exists(),
            "project.db must never be silently recreated"
        );

        let registry = GlobalRegistry::open(&paths.global_db).expect("registry should open");
        let project = registry
            .get_project(outcome.project_id)
            .expect("lookup should succeed")
            .expect("project should still be registered");
        assert_eq!(project.state, RegistryState::Missing);
    }

    #[test]
    fn directory_move_repairs_registry_locator_and_preserves_identity() {
        let global_home = TestDir::create("move-global");
        let paths = global_paths(&global_home);
        let old_workspace = TestDir::create("move-old");
        make_git_repo(&old_workspace);
        let outcome =
            init_workspace(old_workspace.path(), &paths).expect("initial init should succeed");

        // Simulate `mv old new`: physically move the directory (including
        // .brainprint) to a location the registry has never seen. The old
        // path is now genuinely gone -- strong evidence of a move, not a
        // copy.
        let new_path = env::temp_dir().join(format!(
            "brainprint-init-move-new-{}-{}",
            process::id(),
            NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::rename(old_workspace.path(), &new_path).expect("directory move should succeed");

        let moved_outcome = init_workspace(&new_path, &paths)
            .expect("init at the moved location must repair the locator, not conflict");
        assert_eq!(moved_outcome.project_id, outcome.project_id);
        assert_eq!(moved_outcome.workspace_id, outcome.workspace_id);
        assert!(!moved_outcome.freshly_created);

        let registry = GlobalRegistry::open(&paths.global_db).expect("registry should open");
        let project = registry
            .get_project(outcome.project_id)
            .expect("lookup should succeed")
            .expect("project should still be registered");
        assert_eq!(project.home_locator, moved_outcome.workspace_root);
        assert_eq!(project.state, RegistryState::Active);

        let workspace = registry
            .get_workspace(outcome.workspace_id)
            .expect("lookup should succeed")
            .expect("workspace should still be registered");
        assert_eq!(workspace.locator, moved_outcome.workspace_root);

        let _ = fs::remove_dir_all(&new_path);
    }

    #[test]
    fn wrong_db_kind_is_rejected_on_reopen() {
        let global_home = TestDir::create("wrong-kind-global");
        let workspace = TestDir::create("wrong-kind-workspace");
        let paths = global_paths(&global_home);
        let outcome =
            init_workspace(workspace.path(), &paths).expect("initial init should succeed");
        let workspace_paths = WorkspacePaths::from_root(&outcome.workspace_root);

        {
            let connection = Connection::open(&workspace_paths.workspace_db)
                .expect("workspace.db should open directly");
            connection
                .execute(
                    "UPDATE db_meta SET db_kind = ?1 WHERE id = 0",
                    params!["index"],
                )
                .expect("tampering update should succeed");
        }

        let error = init_workspace(workspace.path(), &paths)
            .expect_err("a workspace.db tampered to claim db_kind=index must be rejected");
        assert!(matches!(
            error,
            InitError::Db(DbOpenError::KindMismatch { .. })
        ));
    }

    #[test]
    fn reopen_reconciles_an_orphan_building_generation_without_exposing_it_as_current() {
        use crate::generation::{GenerationState, GenerationStore};

        let global_home = TestDir::create("orphan-generation-global");
        let workspace = TestDir::create("orphan-generation-workspace");
        let paths = global_paths(&global_home);
        let outcome =
            init_workspace(workspace.path(), &paths).expect("initial init should succeed");
        let workspace_paths = WorkspacePaths::from_root(&outcome.workspace_root);

        let orphan_id = {
            let store =
                GenerationStore::open(&workspace_paths.index_db).expect("index.db should reopen");
            store
                .bootstrap_clock("rev-1")
                .expect("bootstrap should succeed");
            let generation = store
                .begin_generation("rev-1")
                .expect("begin should succeed");
            // Left BUILDING here -- simulates a crash before publish/abort.
            generation.id
        };

        // This is exactly what a restarted daemon's next reopen does.
        init_workspace(workspace.path(), &paths).expect("reopen should succeed");

        let store =
            GenerationStore::open(&workspace_paths.index_db).expect("index.db should reopen");
        assert!(
            store
                .current_stable()
                .expect("current lookup should succeed")
                .is_none(),
            "an orphan BUILDING generation must never be exposed as current"
        );
        let reconciled = store
            .get_generation(orphan_id)
            .expect("lookup should succeed")
            .expect("the orphan generation should still exist, just no longer BUILDING");
        assert_eq!(reconciled.state, GenerationState::Aborted);
    }
}
