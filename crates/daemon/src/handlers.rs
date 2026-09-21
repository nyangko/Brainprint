//! Request handlers: the daemon side of `install`/`init` (#15 task 10).
//!
//! Each handler calls straight into the existing `brainprint-engine` API
//! -- `config::bootstrap_global_config`, `registry::GlobalRegistry::open`,
//! `init::init_workspace` -- and never reimplements any of that logic
//! here. The only thing this module owns is mapping an engine error into
//! the wire-safe [`ErrorResponse`] (#15 task 10: a raw `rusqlite`/driver
//! error must never reach the client; an identity/registry conflict the
//! user can act on must).

use brainprint_core::protocol::{ErrorKind, ErrorResponse, InitResponse, InstallResponse};
use brainprint_engine::{
    config::{self, ConfigError},
    init::{self, InitError},
    paths::GlobalPaths,
    registry::{GlobalRegistry, RegistryError},
};

/// Bootstrap/migrate global config + `global.db` (#15 task 10). Idempotent:
/// re-running against an already-installed user reports
/// `*_freshly_created: false` rather than erroring.
pub fn handle_install(global_paths: &GlobalPaths) -> Result<InstallResponse, ErrorResponse> {
    let config_existed = global_paths.config_file.is_file();
    config::bootstrap_global_config(global_paths).map_err(map_config_error)?;

    let db_existed = global_paths.global_db.is_file();
    GlobalRegistry::open(&global_paths.global_db).map_err(map_registry_error)?;

    Ok(InstallResponse {
        global_config_path: global_paths.config_file.display().to_string(),
        global_db_path: global_paths.global_db.display().to_string(),
        config_freshly_created: !config_existed,
        db_freshly_created: !db_existed,
    })
}

/// Init `path` as a Brainprint Workspace by calling straight into
/// `engine::init::init_workspace` (#15 task 6/7 routing, unchanged).
pub fn handle_init(global_paths: &GlobalPaths, path: &str) -> Result<InitResponse, ErrorResponse> {
    let requested_root = std::path::PathBuf::from(path);
    init::init_workspace(&requested_root, global_paths)
        .map(|outcome| InitResponse {
            project_id: outcome.project_id.to_string(),
            workspace_id: outcome.workspace_id.to_string(),
            workspace_root: outcome.workspace_root.display().to_string(),
            is_git: outcome.is_git,
            freshly_created: outcome.freshly_created,
        })
        .map_err(map_init_error)
}

fn map_config_error(error: ConfigError) -> ErrorResponse {
    // `ConfigError`'s `Display` only ever surfaces a path plus an
    // `io::Error`/`toml::de::Error` -- never a raw driver string -- so
    // it's safe to forward as-is.
    log_internal_error("install", &error);
    ErrorResponse {
        kind: ErrorKind::DaemonInternal,
        message: error.to_string(),
    }
}

fn map_registry_error(error: RegistryError) -> ErrorResponse {
    match &error {
        RegistryError::UnknownProject { .. }
        | RegistryError::UnknownWorkspace { .. }
        | RegistryError::ProjectLocatorConflict { .. }
        | RegistryError::WorkspaceIdentityConflict { .. }
        | RegistryError::GitLineageConflict { .. } => ErrorResponse {
            kind: ErrorKind::Conflict,
            message: error.to_string(),
        },
        // `Open`/`Sqlite` can carry a raw rusqlite error inside a
        // `DbOpenError::MigrationFailed`-style variant; never forward that
        // text over the wire. `UnknownState` means a `state` column holds
        // a value this module never wrote -- internal data corruption,
        // not something the user can act on.
        RegistryError::Open(_) | RegistryError::Sqlite(_) | RegistryError::UnknownState { .. } => {
            log_internal_error("install/init", &error);
            generic_internal_error("storage")
        }
    }
}

fn map_init_error(error: InitError) -> ErrorResponse {
    match &error {
        // Identity/registry conflicts and explicit missing/mismatch
        // results the user must actually see (#15 task 7's carefully
        // written, driver-free Display text) -- forwarded verbatim.
        InitError::ProjectIdentityMismatch { .. }
        | InitError::WorkspaceIdentityMismatch { .. }
        | InitError::ProjectHomeMissing { .. }
        | InitError::MalformedIdentity { .. }
        | InitError::UnsupportedIdentityFormat { .. }
        | InitError::CorruptDbMeta { .. } => ErrorResponse {
            kind: ErrorKind::Conflict,
            message: error.to_string(),
        },
        InitError::Registry(registry_error) => match registry_error {
            RegistryError::UnknownProject { .. }
            | RegistryError::UnknownWorkspace { .. }
            | RegistryError::ProjectLocatorConflict { .. }
            | RegistryError::WorkspaceIdentityConflict { .. }
            | RegistryError::GitLineageConflict { .. } => ErrorResponse {
                kind: ErrorKind::Conflict,
                message: registry_error.to_string(),
            },
            RegistryError::Open(_)
            | RegistryError::Sqlite(_)
            | RegistryError::UnknownState { .. } => {
                log_internal_error("init", &error);
                generic_internal_error("storage")
            }
        },
        // Path/config I/O: safe to forward (path + io::Error text only,
        // no driver internals).
        InitError::Io { .. } | InitError::MissingParent { .. } | InitError::Config(_) => {
            log_internal_error("init", &error);
            ErrorResponse {
                kind: ErrorKind::DaemonInternal,
                message: error.to_string(),
            }
        }
        // Can carry a raw rusqlite/toml-encode error, or (Generation) an
        // orphan-generation-reconcile failure that may itself wrap one --
        // generic only.
        InitError::EncodeIdentity { .. }
        | InitError::Db(_)
        | InitError::Sqlite(_)
        | InitError::Generation(_) => {
            log_internal_error("init", &error);
            generic_internal_error("storage")
        }
    }
}

fn generic_internal_error(context: &str) -> ErrorResponse {
    ErrorResponse {
        kind: ErrorKind::DaemonInternal,
        message: format!("internal {context} error; see daemon logs for detail"),
    }
}

/// Foreground/debug stderr only (#13 task 4 §9) -- never sent to a client.
fn log_internal_error(operation: &str, error: &dyn std::error::Error) {
    eprintln!("brainprintd: {operation} failed: {error}");
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs, process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestHome(std::path::PathBuf);

    impl TestHome {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "brainprint-handlers-{label}-{}-{sequence}",
                process::id()
            ));
            fs::create_dir_all(&path).expect("test home should be created");
            Self(path)
        }

        fn global_paths(&self) -> GlobalPaths {
            GlobalPaths::from_home(&self.0)
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn install_creates_global_config_and_db_then_is_idempotent() {
        let home = TestHome::create("install");
        let global_paths = home.global_paths();

        let first = handle_install(&global_paths).expect("first install should succeed");
        assert!(first.config_freshly_created);
        assert!(first.db_freshly_created);
        assert!(global_paths.config_file.is_file());
        assert!(global_paths.global_db.is_file());

        let second = handle_install(&global_paths).expect("second install should succeed");
        assert!(!second.config_freshly_created);
        assert!(!second.db_freshly_created);
    }

    #[test]
    fn init_fresh_non_git_directory_succeeds() {
        let home = TestHome::create("init-fresh");
        let global_paths = home.global_paths();
        let workspace = TestHome::create("init-fresh-workspace");

        let response = handle_init(&global_paths, &workspace.0.to_string_lossy())
            .expect("fresh init should succeed");
        assert!(response.freshly_created);
        assert!(!response.is_git);
    }

    #[test]
    fn init_conflict_is_reported_as_conflict_kind() {
        let home = TestHome::create("init-conflict");
        let global_paths = home.global_paths();
        let workspace = TestHome::create("init-conflict-workspace");
        fs::create_dir_all(workspace.0.join(".git")).expect(".git fixture should be created");

        handle_init(&global_paths, &workspace.0.to_string_lossy())
            .expect("first init should succeed");

        // Tamper with the bound project.db identity to force a mismatch on
        // the next init of the same Workspace.
        let workspace_paths = brainprint_engine::paths::WorkspacePaths::from_root(
            fs::canonicalize(&workspace.0).expect("workspace path should canonicalize"),
        );
        {
            let opened = brainprint_engine::schema::project::open(&workspace_paths.project_db)
                .expect("project.db should reopen");
            let other = brainprint_core::ProjectId::generate();
            opened
                .connection
                .execute(
                    "UPDATE db_meta SET project_uid = ?1 WHERE id = 0",
                    [other.to_bytes().to_vec()],
                )
                .expect("tampering update should succeed");
        }

        let error = handle_init(&global_paths, &workspace.0.to_string_lossy())
            .expect_err("mismatched project.db identity must be reported, not silently accepted");
        assert_eq!(error.kind, ErrorKind::Conflict);
    }
}
