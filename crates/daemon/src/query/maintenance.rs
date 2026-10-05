//! #56 (I7 task 1): `doctor` and `rebuild` (#9 task 5, `LOCKED -- Init /
//! Sync / Rebuild 명령 lifecycle`; #13 task 5 §8, task 8 §17-19).
//!
//! `doctor` opens every database read-only and asks the Workspace worker
//! only for facts it already holds: it never refreshes, migrates,
//! repairs or starts anything. `rebuild` stages a fresh `index.db` beside
//! the live one, validates it, and only then swaps it in; project.db,
//! workspace.db, the registry, config and source are never opened for
//! write by it.
//!
//! #57 (I7 task 2): `sync` runs the explicit verified reconcile on the
//! Workspace worker and reports #55's refresh delta; the normal path stays
//! the watcher's incremental refresh. `uninit` marks the registry entry
//! DETACHED, then stops the runtime; source, `.brainprint/` and every
//! database are left exactly as they are (#13 task 9 §9-11).

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use brainprint_core::{
    BuildInfo, PROTOCOL_VERSION, ProjectId, WorkspaceId,
    protocol::{
        ErrorKind, ErrorResponse, Response,
        maintenance::{
            BackendCheckWire, BackendStateWire, CheckWire, DatabaseCheckWire, DatabaseStateWire,
            DoctorRequest, DoctorResponse, DoctorWorkspaceWire, IndexCheckWire, RebuildRequest,
            RebuildResponse, RuntimeCheckWire, SchemaCheckWire, StoredBasisWire, SyncRequest,
            SyncResponse, UninitRequest, UninitResponse, WatcherCheckWire, WorkspaceDoctorWire,
            WorkspaceStatusReportWire, WorkspaceStatusWire,
        },
        work::IndexBasisWire,
    },
};
use brainprint_engine::{
    config::load_workspace_config,
    db::{self, DbInspection, DbKind, DbOpenError, Migration},
    generation::GenerationStore,
    init,
    paths::{GlobalPaths, WorkspacePaths},
    query::Currentness,
    query_surface::{CoreError, CoreQuerySurface},
    registry::{GlobalRegistry, RegistryState, WorkspaceRegistryEntry},
    scan::BaselineScan,
    schema,
};

use super::{
    PostCommandRefreshStored,
    lifecycle::{INITIAL_WORKSPACE_REVISION, LifecycleStats, WorkspaceLifecycle},
    managed_wire::refresh_wire,
    runtime::DaemonQueryRuntime,
    semantic::{SemanticStats, WorkspaceSemantic},
};

pub async fn handle_doctor(runtime: &DaemonQueryRuntime, request: DoctorRequest) -> Response {
    let global_paths = runtime.global_paths().clone();
    let locator = PathBuf::from(&request.path);
    let diagnosed = tokio::task::spawn_blocking(move || diagnose(&global_paths, &locator)).await;
    let (global_db, diagnosed) = match diagnosed {
        Ok(Ok(diagnosed)) => diagnosed,
        Ok(Err(error)) => return Response::Error(error),
        Err(error) => return Response::Error(internal("doctor", &error)),
    };
    let workspace = match diagnosed {
        Diagnosed::Workspace(workspace, mut report) => {
            (report.runtime, report.index) =
                runtime.health(workspace).await.unwrap_or_else(inactive);
            DoctorWorkspaceWire::Initialized(report)
        }
        Diagnosed::Unresolved(workspace) => workspace,
    };
    let build = BuildInfo::current();
    Response::Doctor(DoctorResponse {
        daemon_version: build.version.to_owned(),
        protocol_version: PROTOCOL_VERSION,
        global_db,
        workspace,
    })
}

/// #70: the compact Workspace status for `status <path>`. Read-only like
/// `doctor`, with the same resolution: the stored basis is read from
/// `index.db` opened read-only, currentness is asked of a runtime that
/// already exists (none is created), and nothing is reconciled, migrated
/// or started. The diagnosis -- schemas, integrity, binding -- stays
/// `doctor`'s.
pub async fn workspace_status(
    runtime: &DaemonQueryRuntime,
    path: &str,
) -> Result<WorkspaceStatusWire, ErrorResponse> {
    let global_paths = runtime.global_paths().clone();
    let locator = PathBuf::from(path);
    let resolved = tokio::task::spawn_blocking(move || {
        let entry = match resolve(&global_paths, &locator)? {
            Ok(entry) => entry,
            Err(unresolved) => return Ok(Err(unresolved)),
        };
        let paths = WorkspacePaths::from_root(&entry.locator);
        let basis = stored_basis(&paths.index_db);
        let semantic = semantic_checks(&global_paths, &entry, &paths);
        Ok(Ok((entry, basis, semantic)))
    })
    .await
    .map_err(|error| internal("status", &error))??;
    let (entry, basis, semantic) = match resolved {
        Ok(found) => found,
        Err(DoctorWorkspaceWire::NotInitialized { path, reason }) => {
            return Ok(WorkspaceStatusWire::NotInitialized { path, reason });
        }
        Err(DoctorWorkspaceWire::Ambiguous { path, workspaces }) => {
            return Ok(WorkspaceStatusWire::Ambiguous { path, workspaces });
        }
        Err(DoctorWorkspaceWire::Initialized(_)) => {
            unreachable!("resolve reports only an unresolved locator")
        }
    };
    let (runtime_check, index) = runtime
        .health(entry.workspace_id)
        .await
        .unwrap_or_else(inactive);
    Ok(WorkspaceStatusWire::Initialized(Box::new(
        WorkspaceStatusReportWire {
            project_id: entry.project_id.to_string(),
            workspace_id: entry.workspace_id.to_string(),
            workspace_root: entry.locator.display().to_string(),
            basis,
            index,
            runtime: runtime_check,
            semantic,
        },
    )))
}

fn inactive() -> (RuntimeCheckWire, IndexCheckWire) {
    (
        RuntimeCheckWire::Inactive,
        IndexCheckWire::NotMeasured {
            reason: "no Workspace runtime is active in this daemon".to_owned(),
        },
    )
}

/// The stored revision clock and STABLE generation, through a read-only
/// connection: nothing is created, migrated or settled.
fn stored_basis(index_db: &Path) -> StoredBasisWire {
    let unreadable = |error: &dyn std::fmt::Display| StoredBasisWire::Unreadable {
        detail: error.to_string(),
    };
    if !index_db.is_file() {
        return unreadable(&format!("{} is missing", index_db.display()));
    }
    let store = match GenerationStore::open_read_only(index_db) {
        Ok(store) => store,
        Err(error) => return unreadable(&error),
    };
    let read = || -> Result<Option<IndexBasisWire>, String> {
        let Some(stable) = store.current_stable().map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let workspace_revision = store
            .current_workspace_revision()
            .map_err(|e| e.to_string())?
            .ok_or("the revision clock is missing")?;
        Ok(Some(IndexBasisWire {
            index_incarnation: store.index_incarnation_id().map_err(|e| e.to_string())?,
            workspace_revision,
            generation_no: stable.generation_no,
            generation_basis_revision: stable.basis_workspace_revision,
        }))
    };
    match read() {
        Ok(Some(basis)) => StoredBasisWire::Stable(basis),
        Ok(None) => StoredBasisWire::NeverPublished,
        Err(detail) => StoredBasisWire::Unreadable { detail },
    }
}

pub async fn handle_rebuild(runtime: &DaemonQueryRuntime, request: RebuildRequest) -> Response {
    let workspace = match runtime.resolve_workspace(PathBuf::from(request.path)).await {
        Ok(workspace) => workspace,
        Err(error) => {
            return Response::Error(ErrorResponse {
                kind: ErrorKind::InvalidRequest,
                message: error.message,
            });
        }
    };
    match runtime.rebuild(workspace).await {
        Ok(rebuilt) => Response::Rebuild(rebuilt),
        Err(message) => {
            eprintln!("brainprintd: workspace {workspace} rebuild failed: {message}");
            Response::Error(ErrorResponse {
                kind: ErrorKind::DaemonInternal,
                message: format!("rebuild failed: {message}"),
            })
        }
    }
}

pub async fn handle_sync(runtime: &DaemonQueryRuntime, request: SyncRequest) -> Response {
    let workspace = match runtime.resolve_workspace(PathBuf::from(request.path)).await {
        Ok(workspace) => workspace,
        Err(error) => {
            return Response::Error(ErrorResponse {
                kind: ErrorKind::InvalidRequest,
                message: error.message,
            });
        }
    };
    let global_db = runtime.global_paths().global_db.clone();
    let entry = match tokio::task::spawn_blocking(move || registered(&global_db, workspace)).await {
        Ok(Ok(entry)) => entry,
        Ok(Err(error)) => return Response::Error(error),
        Err(error) => return Response::Error(internal("sync", &error)),
    };
    match runtime.sync(workspace).await {
        Ok((report, stats)) => Response::Sync(SyncResponse {
            project_id: entry.project_id.to_string(),
            workspace_id: workspace.to_string(),
            workspace_root: entry.locator.display().to_string(),
            refresh: refresh_wire(PostCommandRefreshStored::current(&report)),
            watcher: watcher_check(&stats),
        }),
        Err(error) => {
            eprintln!("brainprintd: workspace {workspace} sync failed: {error}");
            Response::Error(ErrorResponse {
                kind: ErrorKind::DaemonInternal,
                message: format!("sync failed: {error}"),
            })
        }
    }
}

pub async fn handle_uninit(runtime: &DaemonQueryRuntime, request: UninitRequest) -> Response {
    let global_db = runtime.global_paths().global_db.clone();
    let locator = PathBuf::from(request.path);
    let detached = tokio::task::spawn_blocking(move || detach(&global_db, &locator)).await;
    let (entry, already_detached) = match detached {
        Ok(Ok(detached)) => detached,
        Ok(Err(error)) => return Response::Error(error),
        Err(error) => return Response::Error(internal("uninit", &error)),
    };
    // After the registry write: a request racing this one can no longer
    // bind the Workspace, so no runtime outlives the detach.
    let runtime_stopped = runtime.detach(entry.workspace_id).await;
    Response::Uninit(UninitResponse {
        project_id: entry.project_id.to_string(),
        workspace_id: entry.workspace_id.to_string(),
        workspace_root: entry.locator.display().to_string(),
        is_project_home: entry.is_project_home,
        already_detached,
        runtime_stopped,
    })
}

fn registered(
    global_db: &Path,
    workspace: WorkspaceId,
) -> Result<WorkspaceRegistryEntry, ErrorResponse> {
    GlobalRegistry::open(global_db)
        .and_then(|registry| registry.get_workspace(workspace))
        .map_err(|error| internal("registry", &error))?
        .ok_or_else(|| ErrorResponse {
            kind: ErrorKind::Conflict,
            message: format!("workspace {workspace} is no longer registered"),
        })
}

/// Mark the one Workspace at `locator` DETACHED. Only the registry row's
/// state changes; an already detached one is left as it is.
fn detach(
    global_db: &Path,
    locator: &Path,
) -> Result<(WorkspaceRegistryEntry, bool), ErrorResponse> {
    let not_initialized = || ErrorResponse {
        kind: ErrorKind::InvalidRequest,
        message: format!(
            "not initialized: no Workspace is registered at {}",
            locator.display()
        ),
    };
    if !global_db.is_file() {
        return Err(not_initialized());
    }
    let registry = GlobalRegistry::open(global_db).map_err(|error| internal("registry", &error))?;
    let (detached, managed): (Vec<_>, Vec<_>) = registry
        .find_by_locator(locator)
        .map_err(|error| internal("registry", &error))?
        .workspaces
        .into_iter()
        .partition(|entry| entry.state == RegistryState::Detached);
    match (managed.as_slice(), detached.as_slice()) {
        ([one], _) => registry
            .mark_workspace_detached(one.workspace_id)
            .map(|entry| (entry, false))
            .map_err(|error| internal("registry", &error)),
        ([], [one]) => Ok((one.clone(), true)),
        ([], []) => Err(not_initialized()),
        _ => Err(ErrorResponse {
            kind: ErrorKind::InvalidRequest,
            message: format!(
                "{} Workspaces are registered at {}",
                managed.len() + detached.len(),
                locator.display()
            ),
        }),
    }
}

/// The worker's own runtime facts for `doctor`, read without settling.
pub(super) fn runtime_health(
    lifecycle: &Result<WorkspaceLifecycle, String>,
) -> (RuntimeCheckWire, IndexCheckWire) {
    let lifecycle = match lifecycle {
        Ok(lifecycle) => lifecycle,
        Err(error) => {
            return (
                RuntimeCheckWire::Unavailable {
                    detail: error.clone(),
                },
                IndexCheckWire::NotMeasured {
                    reason: "the Workspace runtime is unavailable".to_owned(),
                },
            );
        }
    };
    let watcher = watcher_check(&lifecycle.stats());
    let index = if !lifecycle.is_activated() {
        IndexCheckWire::NotMeasured {
            reason: "the Workspace runtime is not activated yet".to_owned(),
        }
    } else if !matches!(watcher, WatcherCheckWire::Attached) {
        // #70: without a watcher, currentness is proven by each query's
        // reconcile; the last proof is not a claim about the files now.
        IndexCheckWire::NotMeasured {
            reason: "no watcher is attached: currentness is proven at query time".to_owned(),
        }
    } else {
        index_check(lifecycle)
    };
    (RuntimeCheckWire::Active { watcher }, index)
}

fn watcher_check(stats: &LifecycleStats) -> WatcherCheckWire {
    if !stats.activated {
        WatcherCheckWire::NotStarted
    } else if stats.watcher_attached {
        WatcherCheckWire::Attached
    } else {
        WatcherCheckWire::Unavailable {
            reason: stats
                .watcher_error
                .clone()
                .unwrap_or_else(|| "not attached".to_owned()),
        }
    }
}

fn index_check(lifecycle: &WorkspaceLifecycle) -> IndexCheckWire {
    match lifecycle.currentness() {
        Ok(Currentness::Current) => IndexCheckWire::Current,
        Ok(Currentness::NotCurrent(reason)) => IndexCheckWire::NotCurrent {
            detail: format!("{reason:?}"),
        },
        Err(detail) => IndexCheckWire::NotCurrent { detail },
    }
}

fn backends(stats: &SemanticStats) -> Vec<BackendCheckWire> {
    let mut backends: Vec<_> = stats
        .registered
        .iter()
        .map(|family| BackendCheckWire {
            family: (*family).to_owned(),
            state: BackendStateWire::Registered,
        })
        .chain(
            stats
                .unavailable
                .iter()
                .map(|(family, reason)| BackendCheckWire {
                    family: (*family).to_owned(),
                    state: BackendStateWire::Unavailable {
                        reason: reason.clone(),
                    },
                }),
        )
        .collect();
    backends.sort_by(|left, right| left.family.cmp(&right.family));
    backends
}

enum Diagnosed {
    Workspace(WorkspaceId, Box<WorkspaceDoctorWire>),
    Unresolved(DoctorWorkspaceWire),
}

/// The registry entry at `locator`, or the typed reason there is none --
/// one resolution for `doctor` and `status`.
fn resolve(
    global_paths: &GlobalPaths,
    locator: &Path,
) -> Result<Result<WorkspaceRegistryEntry, DoctorWorkspaceWire>, ErrorResponse> {
    let path = locator.display().to_string();
    let workspace = match CoreQuerySurface::resolve_workspace(&global_paths.global_db, locator) {
        Ok(workspace) => workspace,
        Err(error @ CoreError::NotInitialized(_)) => {
            let reason = error.to_string();
            return Ok(Err(DoctorWorkspaceWire::NotInitialized { path, reason }));
        }
        Err(CoreError::WorkspaceLocatorAmbiguous { workspaces }) => {
            let workspaces = workspaces.iter().map(ToString::to_string).collect();
            return Ok(Err(DoctorWorkspaceWire::Ambiguous { path, workspaces }));
        }
        Err(error) => return Err(internal("workspace resolution", &error)),
    };
    registered(&global_paths.global_db, workspace).map(Ok)
}

/// Each semantic backend family's availability, as configured. Loading
/// the configuration starts nothing.
fn semantic_checks(
    global_paths: &GlobalPaths,
    entry: &WorkspaceRegistryEntry,
    paths: &WorkspacePaths,
) -> Vec<BackendCheckWire> {
    match load_workspace_config(paths) {
        Ok(config) => backends(
            &WorkspaceSemantic::load(
                global_paths,
                &config,
                entry.workspace_id,
                &entry.locator,
                &paths.index_db,
            )
            .stats(),
        ),
        Err(error) => vec![BackendCheckWire {
            family: "workspace config".to_owned(),
            state: BackendStateWire::Unavailable {
                reason: format!("unreadable: {error}"),
            },
        }],
    }
}

/// Every read-only check that needs no Workspace worker.
fn diagnose(
    global_paths: &GlobalPaths,
    locator: &Path,
) -> Result<(DatabaseCheckWire, Diagnosed), ErrorResponse> {
    let (global_db, _) = database_check(
        DbKind::Global,
        &global_paths.global_db,
        schema::global::GLOBAL_MIGRATIONS,
    );
    let entry = match resolve(global_paths, locator)? {
        Ok(entry) => entry,
        Err(unresolved) => return Ok((global_db, Diagnosed::Unresolved(unresolved))),
    };
    let workspace = entry.workspace_id;
    let registry = GlobalRegistry::open(&global_paths.global_db)
        .map_err(|error| internal("doctor registry", &error))?;
    let project_home = if entry.is_project_home {
        Some(entry.locator.clone())
    } else {
        registry
            .get_project(entry.project_id)
            .map_err(|error| internal("doctor registry", &error))?
            .map(|project| project.home_locator)
    };

    let paths = WorkspacePaths::from_root(&entry.locator);
    let (project_db, project_meta) = match &project_home {
        Some(home) => database_check(
            DbKind::Project,
            &WorkspacePaths::from_root(home).project_db,
            schema::project::PROJECT_MIGRATIONS,
        ),
        None => (
            DatabaseCheckWire {
                kind: DbKind::Project.as_str().to_owned(),
                path: String::new(),
                state: DatabaseStateWire::Unreadable {
                    detail: "the Project's home is not registered".to_owned(),
                },
            },
            None,
        ),
    };
    let (workspace_db, workspace_meta) = database_check(
        DbKind::Workspace,
        &paths.workspace_db,
        schema::workspace::WORKSPACE_MIGRATIONS,
    );
    let (index_db, index_meta) = database_check(
        DbKind::Index,
        &paths.index_db,
        schema::index::INDEX_MIGRATIONS,
    );

    let identity = init::read_workspace_identity(&paths).map_err(|error| error.to_string());
    let binding = binding_check(
        &entry,
        identity,
        &[
            (DbKind::Project, project_meta.as_ref()),
            (DbKind::Workspace, workspace_meta.as_ref()),
            (DbKind::Index, index_meta.as_ref()),
        ],
    );

    let semantic = semantic_checks(global_paths, &entry, &paths);

    let report = WorkspaceDoctorWire {
        project_id: entry.project_id.to_string(),
        workspace_id: workspace.to_string(),
        workspace_root: entry.locator.display().to_string(),
        is_project_home: entry.is_project_home,
        binding,
        databases: vec![project_db, workspace_db, index_db],
        // Filled from the worker, if one exists.
        index: IndexCheckWire::NotMeasured {
            reason: String::new(),
        },
        runtime: RuntimeCheckWire::Inactive,
        semantic,
    };
    Ok((global_db, Diagnosed::Workspace(workspace, Box::new(report))))
}

fn database_check(
    kind: DbKind,
    path: &Path,
    migrations: &[Migration],
) -> (DatabaseCheckWire, Option<DbInspection>) {
    let not_inspected = |why: &str| CheckWire::NotMeasured {
        reason: why.to_owned(),
    };
    let (state, inspection) = if !path.is_file() {
        (DatabaseStateWire::Missing, None)
    } else {
        match db::inspect(path, kind, migrations) {
            Ok(inspection) => {
                let schema = if inspection.schema_version == inspection.max_known_version {
                    SchemaCheckWire::Current {
                        version: inspection.schema_version,
                    }
                } else {
                    SchemaCheckWire::MigrationPending {
                        version: inspection.schema_version,
                        latest: inspection.max_known_version,
                    }
                };
                let integrity = if inspection.integrity == ["ok"] {
                    CheckWire::Ok
                } else {
                    // SQLite's own text stays in the daemon log.
                    eprintln!(
                        "brainprintd: doctor {kind}.db integrity_check: {:?}",
                        inspection.integrity
                    );
                    CheckWire::Failed {
                        detail: format!(
                            "{} integrity problem(s); see daemon log",
                            inspection.integrity.len()
                        ),
                    }
                };
                (
                    DatabaseStateWire::Opened { schema, integrity },
                    Some(inspection),
                )
            }
            Err(DbOpenError::FutureSchema {
                db_version,
                max_known_version,
            }) => (
                DatabaseStateWire::Opened {
                    schema: SchemaCheckWire::Newer {
                        version: db_version,
                        latest: max_known_version,
                    },
                    integrity: not_inspected("the schema is newer than this binary"),
                },
                None,
            ),
            Err(
                error @ (DbOpenError::ChecksumMismatch { .. }
                | DbOpenError::UnknownAppliedVersion { .. }),
            ) => (
                DatabaseStateWire::Opened {
                    schema: SchemaCheckWire::LedgerMismatch {
                        detail: error.to_string(),
                    },
                    integrity: not_inspected("the migration ledger does not match"),
                },
                None,
            ),
            Err(error @ DbOpenError::KindMismatch { .. }) => (
                DatabaseStateWire::Unreadable {
                    detail: error.to_string(),
                },
                None,
            ),
            Err(error) => {
                eprintln!("brainprintd: doctor {kind}.db: {error}");
                (
                    DatabaseStateWire::Unreadable {
                        detail: format!("cannot be read as a {kind} database; see daemon log"),
                    },
                    None,
                )
            }
        }
    };
    let check = DatabaseCheckWire {
        kind: kind.as_str().to_owned(),
        path: path.display().to_string(),
        state,
    };
    (check, inspection)
}

/// The registry entry against `workspace.toml` and every Workspace-scoped
/// database's `db_meta` binding.
fn binding_check(
    entry: &WorkspaceRegistryEntry,
    identity: Result<Option<(ProjectId, WorkspaceId)>, String>,
    databases: &[(DbKind, Option<&DbInspection>)],
) -> CheckWire {
    let failed = |detail: String| CheckWire::Failed { detail };
    match identity {
        Err(error) => return failed(format!("workspace.toml: {error}")),
        Ok(None) => return failed("workspace.toml is missing".to_owned()),
        Ok(Some(found)) if found != (entry.project_id, entry.workspace_id) => {
            return failed(format!(
                "workspace.toml names project {} / workspace {}, the registry {} / {}",
                found.0, found.1, entry.project_id, entry.workspace_id
            ));
        }
        Ok(Some(_)) => {}
    }
    let project = entry.project_id.to_bytes();
    let workspace = entry.workspace_id.to_bytes();
    let mut unverified = Vec::new();
    for (kind, inspection) in databases {
        let Some(inspection) = inspection else {
            unverified.push(kind.as_str());
            continue;
        };
        if inspection.project_uid.as_deref() != Some(&project[..]) {
            return failed(format!(
                "{kind}.db is not bound to project {}",
                entry.project_id
            ));
        }
        if *kind != DbKind::Project && inspection.workspace_uid.as_deref() != Some(&workspace[..]) {
            return failed(format!(
                "{kind}.db is not bound to workspace {}",
                entry.workspace_id
            ));
        }
    }
    if unverified.is_empty() {
        CheckWire::Ok
    } else {
        CheckWire::NotMeasured {
            reason: format!("not inspectable: {}", unverified.join(", ")),
        }
    }
}

fn internal(context: &str, error: &dyn std::fmt::Display) -> ErrorResponse {
    eprintln!("brainprintd: {context} failed: {error}");
    ErrorResponse {
        kind: ErrorKind::DaemonInternal,
        message: format!("internal {context} error; see daemon logs for detail"),
    }
}

/// A fully published, validated index waiting at `staging` to replace
/// `paths.index_db`.
pub(super) struct Staged {
    paths: WorkspacePaths,
    staging: PathBuf,
    retired: PathBuf,
    project_id: ProjectId,
    resources: u64,
    generation_no: i64,
}

/// Build the fresh index beside the live one: the same `db_meta` binding
/// as `init` writes, then the structural baseline (Resources, Symbols,
/// Relations in one generation). The live index is not touched; a
/// failure removes the staging files and nothing else.
pub(super) fn stage(global_db: &Path, workspace: WorkspaceId) -> Result<Staged, String> {
    let entry = GlobalRegistry::open(global_db)
        .and_then(|registry| registry.get_workspace(workspace))
        .map_err(|error| format!("workspace registry: {error}"))?
        .ok_or_else(|| "workspace is not registered".to_owned())?;
    let root = entry
        .locator
        .canonicalize()
        .map_err(|error| format!("workspace root {}: {error}", entry.locator.display()))?;
    let paths = WorkspacePaths::from_root(&entry.locator);
    if !paths.index_db.is_file() {
        return Err(format!("{} is missing", paths.index_db.display()));
    }
    let config =
        load_workspace_config(&paths).map_err(|error| format!("workspace config: {error}"))?;

    let staging = sibling(&paths.index_db, ".rebuild");
    let retired = sibling(&paths.index_db, ".rebuild-old");

    let built = (|| {
        // A leftover from an interrupted rebuild is this rebuild's own.
        remove_db_files(&staging)?;
        init::create_bound_index_db(&staging, entry.project_id, workspace)
            .map_err(|error| format!("staging index.db: {error}"))?;
        let scan =
            BaselineScan::open(&staging).map_err(|error| format!("staging index.db: {error}"))?;
        let report = scan
            .run_initial_scan(&root, &config, INITIAL_WORKSPACE_REVISION)
            .map_err(|error| format!("structural baseline: {error}"))?;
        // #13 task 8 §18: never swap in an index without a stable
        // generation that is the one just published.
        let stable = scan
            .current_stable()
            .map_err(|error| format!("stable generation: {error}"))?;
        if stable.as_ref().map(|generation| generation.id) != Some(report.generation.id) {
            return Err("the staged index has no stable generation".to_owned());
        }
        Ok(report)
    })();
    match built {
        Ok(report) => Ok(Staged {
            paths,
            staging,
            retired,
            project_id: entry.project_id,
            resources: report.changes.len() as u64,
            generation_no: report.generation.generation_no,
        }),
        Err(error) => {
            let _ = remove_db_files(&staging);
            Err(format!("{error}; the previous index is unchanged"))
        }
    }
}

impl Staged {
    /// Retire the live index and put the staged one in its place. Every
    /// connection the worker held on it must already be closed. On
    /// failure whatever moved is moved back.
    pub(super) fn swap_in(&self) -> Result<(), String> {
        remove_db_files(&self.retired)?;
        move_db_files(&self.paths.index_db, &self.retired)?;
        if let Err(error) = move_db_files(&self.staging, &self.paths.index_db) {
            let restored = move_db_files(&self.retired, &self.paths.index_db);
            let _ = remove_db_files(&self.staging);
            return Err(match restored {
                Ok(()) => format!("{error}; the previous index is unchanged"),
                Err(restore) => format!("{error}; restoring the previous index failed: {restore}"),
            });
        }
        Ok(())
    }

    /// Undo a completed swap: drop the new index, restore the retired one.
    pub(super) fn swap_back(&self) -> Result<(), String> {
        remove_db_files(&self.paths.index_db)?;
        move_db_files(&self.retired, &self.paths.index_db)
    }

    pub(super) fn discard_retired(&self) {
        if let Err(error) = remove_db_files(&self.retired) {
            eprintln!("brainprintd: retired index cleanup: {error}");
        }
    }

    pub(super) fn response(
        &self,
        workspace: WorkspaceId,
        lifecycle: &Result<WorkspaceLifecycle, String>,
        semantic: Option<&WorkspaceSemantic>,
    ) -> RebuildResponse {
        let (runtime, index) = runtime_health(lifecycle);
        let watcher = match runtime {
            RuntimeCheckWire::Active { watcher } => watcher,
            RuntimeCheckWire::Inactive | RuntimeCheckWire::Unavailable { .. } => {
                WatcherCheckWire::NotStarted
            }
        };
        RebuildResponse {
            project_id: self.project_id.to_string(),
            workspace_id: workspace.to_string(),
            workspace_root: self.paths.workspace_root.display().to_string(),
            resources: self.resources,
            generation_no: self.generation_no,
            index,
            watcher,
            semantic: semantic.map_or_else(Vec::new, |semantic| backends(&semantic.stats())),
        }
    }
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// A database and its WAL sidecars, which only ever move together: a
/// `-wal` left beside a different main file would be replayed into it.
fn db_files(path: &Path) -> [PathBuf; 3] {
    let with = |suffix: &str| {
        let mut name = OsString::from(path.as_os_str());
        name.push(suffix);
        PathBuf::from(name)
    };
    [path.to_path_buf(), with("-wal"), with("-shm")]
}

fn move_db_files(from: &Path, to: &Path) -> Result<(), String> {
    let (from, to) = (db_files(from), db_files(to));
    let mut moved = Vec::new();
    for (source, target) in from.iter().zip(&to) {
        if !source.exists() {
            continue;
        }
        if let Err(error) = fs::rename(source, target) {
            for (source, target) in moved.into_iter().rev() {
                let _ = fs::rename(target, source);
            }
            return Err(format!(
                "moving {} to {}: {error}",
                source.display(),
                target.display()
            ));
        }
        moved.push((source, target));
    }
    Ok(())
}

fn remove_db_files(path: &Path) -> Result<(), String> {
    for file in db_files(path) {
        match fs::remove_file(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("removing {}: {error}", file.display())),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_moves_each_database_with_its_wal_and_back() {
        let root = std::env::temp_dir().join(format!("bp-i7t1-swap-{}", std::process::id()));
        let paths = WorkspacePaths::from_root(&root);
        fs::create_dir_all(&paths.data_dir).expect("data dir");
        let wal = |path: &Path| sibling(path, "-wal");
        fs::write(&paths.index_db, "old").expect("old");
        fs::write(wal(&paths.index_db), "old-wal").expect("old wal");
        let staged = Staged {
            staging: sibling(&paths.index_db, ".rebuild"),
            retired: sibling(&paths.index_db, ".rebuild-old"),
            paths: paths.clone(),
            project_id: ProjectId::generate(),
            resources: 0,
            generation_no: 1,
        };
        fs::write(&staged.staging, "new").expect("new");

        staged.swap_in().expect("swap in");
        assert_eq!(fs::read_to_string(&paths.index_db).expect("index"), "new");
        // The old WAL never stays beside the new main file.
        assert!(!wal(&paths.index_db).exists());
        assert!(!staged.staging.exists());

        staged.swap_back().expect("swap back");
        assert_eq!(fs::read_to_string(&paths.index_db).expect("index"), "old");
        assert_eq!(
            fs::read_to_string(wal(&paths.index_db)).expect("wal"),
            "old-wal"
        );
        assert!(!staged.retired.exists());
        let _ = fs::remove_dir_all(&root);
    }
}
