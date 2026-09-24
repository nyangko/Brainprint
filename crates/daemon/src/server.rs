//! Daemon server: singleton bind with stale-artifact recovery, and the
//! accept loop dispatching `Handshake`/`Status` requests (#15 task 9 /
//! #13 task 4 §6-7, §14).
//!
//! [`Server::bind`] implements #13 task 4 §6's five-step singleton
//! sequence: acquire a user-scoped lock, bind the local endpoint, and on
//! any conflict, run a *real protocol handshake* against whatever is
//! already there before ever deciding it's safe to clean up and rebind.
//! It never kills a process or deletes another user's artifact based on a
//! PID file alone -- liveness is always decided by a live handshake.

use std::{error::Error, fmt, io, path::Path, sync::Arc, time::Duration};

use brainprint_core::{
    BuildInfo,
    protocol::{
        self, ErrorKind, ErrorResponse, HandshakeResponse, Request, Response, transport::Listener,
    },
};
use brainprint_engine::paths::GlobalPaths;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    client, handlers,
    query::DaemonQueryRuntime,
    runtime_paths::{self, RuntimeEndpoint},
    state::DaemonState,
};

/// Failure starting the daemon.
#[derive(Debug)]
pub enum StartError {
    Io(io::Error),
    /// A live daemon already owns this endpoint -- confirmed by a real
    /// handshake, not inferred from a PID/lock file's mere existence.
    AlreadyRunning,
}

impl fmt::Display for StartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(formatter, "failed to start daemon: {source}"),
            Self::AlreadyRunning => {
                formatter.write_str("a brainprintd instance is already running for this user")
            }
        }
    }
}

impl Error for StartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::AlreadyRunning => None,
        }
    }
}

impl From<io::Error> for StartError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

/// A bound daemon endpoint, ready to [`Server::serve`].
#[derive(Debug)]
pub struct Server {
    listener: Listener,
    endpoint: RuntimeEndpoint,
    state: DaemonState,
    global_paths: GlobalPaths,
    query_runtime: Arc<DaemonQueryRuntime>,
}

impl Server {
    /// Acquire the singleton lock, bind the local IPC endpoint (recovering
    /// from stale runtime artifacts left by a crashed previous instance),
    /// and prepare to serve.
    pub async fn bind(global_paths: &GlobalPaths) -> Result<Self, StartError> {
        let endpoint = runtime_paths::resolve(global_paths);
        ensure_runtime_dir(&endpoint.runtime_root)?;

        acquire_lock_or_detect_stale(&endpoint).await?;

        let listener = match bind_listener(&endpoint) {
            Ok(listener) => listener,
            // Unix: a second `bind()` on an already-present socket path
            // fails with `AddrInUse`. Windows: `first_pipe_instance(true)`
            // on an already-claimed pipe name fails with
            // `PermissionDenied` (`ERROR_ACCESS_DENIED`), not an
            // `AddrInUse`-equivalent -- both mean the same thing here
            // ("this endpoint name is already actively claimed"), so both
            // get the same one-shot stale-recovery pass.
            Err(source)
                if source.kind() == io::ErrorKind::AddrInUse
                    || (cfg!(windows) && source.kind() == io::ErrorKind::PermissionDenied) =>
            {
                // The lock was free, but a socket/pipe artifact is still
                // here -- e.g. a crash that dropped the lock file (or
                // never wrote one) but left the endpoint behind. One more
                // stale-recovery pass before giving up.
                remove_stale_socket_artifact(&endpoint);
                bind_listener(&endpoint)?
            }
            Err(source) => return Err(source.into()),
        };

        Ok(Self {
            listener,
            endpoint,
            state: DaemonState::new(),
            query_runtime: Arc::new(DaemonQueryRuntime::new(global_paths.global_db.clone())),
            global_paths: global_paths.clone(),
        })
    }

    /// Accept connections until cancelled (e.g. by a `Ctrl+C` future
    /// raced against this with `tokio::select!`). Each connection is
    /// handled concurrently, so multiple clients can handshake/query
    /// status/install/init at once.
    pub async fn serve(&mut self) -> io::Result<()> {
        loop {
            let connection = self.listener.accept().await?;
            let state = self.state;
            let global_paths = self.global_paths.clone();
            let query_runtime = Arc::clone(&self.query_runtime);
            tokio::spawn(async move {
                // A single client's connection failing must never take
                // down the daemon or any other client's connection.
                let _ = handle_connection(connection, state, global_paths, query_runtime).await;
            });
        }
    }

    /// Remove this instance's runtime artifacts (socket/pipe file, lock
    /// file). Call after a clean shutdown; a crash simply leaves them for
    /// the next start's stale-recovery pass (#13 task 4 §14-15).
    pub fn cleanup(&self) {
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(&self.endpoint.socket_path);
        }
        let _ = std::fs::remove_file(&self.endpoint.lock_path);
    }
}

async fn handle_connection<S>(
    mut connection: S,
    state: DaemonState,
    global_paths: GlobalPaths,
    query_runtime: Arc<DaemonQueryRuntime>,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let build = BuildInfo::current();

    // The first message on every connection must be a Handshake -- a
    // client that skips it is rejected explicitly, never served as if it
    // had handshaken successfully.
    let first: Request = match protocol::framing::read_message(&mut connection).await {
        Ok(request) => request,
        Err(source) if source.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
        Err(source) => return Err(source),
    };

    let Request::Handshake(handshake_request) = first else {
        protocol::framing::write_message(
            &mut connection,
            &Response::Error(ErrorResponse {
                kind: ErrorKind::InvalidRequest,
                message: "the first message on a connection must be Handshake".to_owned(),
            }),
        )
        .await?;
        return Ok(());
    };

    let handshake_response = if handshake_request.protocol_version == build.protocol_version {
        HandshakeResponse::Ok {
            protocol_version: build.protocol_version,
            daemon_version: build.version.to_owned(),
        }
    } else {
        HandshakeResponse::VersionMismatch {
            server_protocol_version: build.protocol_version,
            client_protocol_version: handshake_request.protocol_version,
        }
    };
    let handshake_ok = matches!(handshake_response, HandshakeResponse::Ok { .. });
    protocol::framing::write_message(&mut connection, &Response::Handshake(handshake_response))
        .await?;
    if !handshake_ok {
        // A version-mismatched client never gets to send further requests
        // on this connection.
        return Ok(());
    }

    loop {
        let request: Request = match protocol::framing::read_message(&mut connection).await {
            Ok(request) => request,
            Err(source) if source.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(source) => return Err(source),
        };

        let response = match request {
            Request::Handshake(_) => Response::Handshake(HandshakeResponse::Ok {
                protocol_version: build.protocol_version,
                daemon_version: build.version.to_owned(),
            }),
            Request::Status(_) => Response::Status(state.status()),
            Request::Install(_) => {
                let global_paths = global_paths.clone();
                match run_blocking(move || handlers::handle_install(&global_paths)).await {
                    Ok(install) => Response::Install(install),
                    Err(error) => Response::Error(error),
                }
            }
            Request::Init(init_request) => {
                let global_paths = global_paths.clone();
                match run_blocking(move || handlers::handle_init(&global_paths, &init_request.path))
                    .await
                {
                    Ok(init) => Response::Init(init),
                    Err(error) => Response::Error(error),
                }
            }
            Request::Query(query_request) => {
                crate::query::handle_query(&query_runtime, query_request).await
            }
            Request::QueryAck(ack_request) => {
                crate::query::handle_query_ack(&query_runtime, ack_request).await
            }
        };
        protocol::framing::write_message(&mut connection, &response).await?;
    }
}

/// `handlers::handle_install`/`handle_init` call straight into synchronous
/// `brainprint-engine` (filesystem + SQLite) work -- run it off the async
/// executor so one slow request never stalls this daemon's other
/// connections.
async fn run_blocking<T, F>(f: F) -> Result<T, ErrorResponse>
where
    F: FnOnce() -> Result<T, ErrorResponse> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.unwrap_or_else(|_| {
        Err(ErrorResponse {
            kind: ErrorKind::DaemonInternal,
            message: "internal error: request handler task panicked".to_owned(),
        })
    })
}

fn bind_listener(endpoint: &RuntimeEndpoint) -> io::Result<Listener> {
    #[cfg(unix)]
    {
        // The socket's parent may be the short-path fallback directory
        // (see `runtime_paths::unix_socket_path`), not `runtime_root`
        // itself, so it needs its own creation/permission pass here.
        if let Some(parent) = endpoint.socket_path.parent() {
            ensure_runtime_dir(parent)?;
        }
        Listener::bind(&endpoint.socket_path)
    }
    #[cfg(windows)]
    Listener::bind(&endpoint.pipe_name)
}

/// #13 task 4 §6: lock → bind → (on conflict) real handshake → stale
/// judgment → cleanup/rebind only when it is actually safe.
async fn acquire_lock_or_detect_stale(endpoint: &RuntimeEndpoint) -> Result<(), StartError> {
    match create_lock_file(&endpoint.lock_path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
            let is_live =
                tokio::time::timeout(LIVENESS_PROBE_TIMEOUT, client::probe_live(endpoint))
                    .await
                    .unwrap_or(false);
            if is_live {
                return Err(StartError::AlreadyRunning);
            }

            // Confirmed stale: nothing answered a real handshake. Safe to
            // clean up and take over.
            let _ = std::fs::remove_file(&endpoint.lock_path);
            remove_stale_socket_artifact(endpoint);
            create_lock_file(&endpoint.lock_path).map_err(StartError::Io)
        }
        Err(source) => Err(StartError::Io(source)),
    }
}

fn create_lock_file(path: &Path) -> io::Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    // PID is diagnostic only (#13 task 4 §6): liveness is always decided
    // by `client::probe_live`'s real handshake, never by reading this back.
    write!(file, "{}", std::process::id())
}

fn remove_stale_socket_artifact(endpoint: &RuntimeEndpoint) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(&endpoint.socket_path);
    }
    // Windows named pipes have no filesystem artifact: the OS releases the
    // pipe name automatically once the crashed process's handles are torn
    // down, so there is nothing to remove here.
    #[cfg(windows)]
    {
        let _ = endpoint;
    }
}

fn ensure_runtime_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Bounds [`acquire_lock_or_detect_stale`]'s liveness probe so a
/// genuinely unresponsive (not just absent) endpoint does not block
/// startup indefinitely.
const LIVENESS_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

#[cfg(test)]
mod tests {
    use std::{
        env, fs, process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use brainprint_engine::paths::GlobalPaths;

    use super::*;

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestHome(std::path::PathBuf);

    impl TestHome {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "brainprint-daemon-{label}-{}-{sequence}",
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

    async fn send(
        connection: &mut brainprint_core::protocol::ClientConnection,
        request: Request,
    ) -> Response {
        protocol::framing::write_message(connection, &request)
            .await
            .expect("write should succeed");
        protocol::framing::read_message(connection)
            .await
            .expect("read should succeed")
    }

    /// Real `git` commands for worktree lineage tests, duplicated from
    /// `brainprint-engine`'s own `init.rs` test helpers of the same shape
    /// (different crate, same minimal fixture need).
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

    fn real_git_repo(workspace: &Path) {
        run_git(&["init", "-q"], workspace);
        run_git(&["commit", "-q", "--allow-empty", "-m", "init"], workspace);
    }

    fn add_worktree(main_repo: &Path, worktree: &Path, branch: &str) {
        run_git(
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                &worktree.to_string_lossy(),
            ],
            main_repo,
        );
    }

    #[tokio::test]
    async fn bind_then_handshake_then_status_succeeds() {
        let home = TestHome::create("basic");
        let global_paths = home.global_paths();
        let mut server = Server::bind(&global_paths)
            .await
            .expect("bind should succeed");
        let endpoint = runtime_paths::resolve(&global_paths);

        let serve_task = tokio::spawn(async move {
            let _ = server.serve().await;
        });

        let mut connection = client::connect(&endpoint)
            .await
            .expect("connect should succeed");
        let daemon_version = client::handshake(&mut connection, "test-client")
            .await
            .expect("handshake should succeed");
        assert_eq!(daemon_version, BuildInfo::current().version);

        let status = client::status(&mut connection)
            .await
            .expect("status should succeed");
        assert_eq!(
            status.protocol_version,
            BuildInfo::current().protocol_version
        );
        assert_eq!(status.pid, process::id());

        serve_task.abort();
    }

    #[tokio::test]
    async fn version_mismatch_is_explicitly_rejected() {
        let home = TestHome::create("version-mismatch");
        let global_paths = home.global_paths();
        let mut server = Server::bind(&global_paths)
            .await
            .expect("bind should succeed");
        let endpoint = runtime_paths::resolve(&global_paths);

        let serve_task = tokio::spawn(async move {
            let _ = server.serve().await;
        });

        let mut connection = client::connect(&endpoint)
            .await
            .expect("connect should succeed");
        let request = Request::Handshake(brainprint_core::protocol::HandshakeRequest {
            protocol_version: BuildInfo::current().protocol_version + 1,
            client_kind: "test-client".to_owned(),
        });
        protocol::framing::write_message(&mut connection, &request)
            .await
            .expect("write should succeed");
        let response: Response = protocol::framing::read_message(&mut connection)
            .await
            .expect("read should succeed");

        assert!(matches!(
            response,
            Response::Handshake(HandshakeResponse::VersionMismatch { .. })
        ));

        serve_task.abort();
    }

    #[tokio::test]
    async fn connecting_when_no_daemon_is_running_is_reported_explicitly() {
        let home = TestHome::create("not-running");
        let global_paths = home.global_paths();
        let endpoint = runtime_paths::resolve(&global_paths);

        let error = client::connect(&endpoint)
            .await
            .expect_err("connecting with no daemon running must fail");
        assert!(matches!(error, client::ClientError::DaemonNotRunning(_)));
    }

    #[tokio::test]
    async fn starting_a_second_instance_is_rejected_as_already_running() {
        let home = TestHome::create("already-running");
        let global_paths = home.global_paths();
        let mut first = Server::bind(&global_paths)
            .await
            .expect("first bind should succeed");
        let serve_task = tokio::spawn(async move {
            let _ = first.serve().await;
        });
        // Give the accept loop a moment to actually be listening.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let error = Server::bind(&global_paths)
            .await
            .expect_err("a second instance must not bind successfully");
        assert!(matches!(error, StartError::AlreadyRunning));

        serve_task.abort();
    }

    #[tokio::test]
    async fn stale_runtime_artifacts_are_recovered_on_next_start() {
        let home = TestHome::create("stale-recovery");
        let global_paths = home.global_paths();
        let endpoint = runtime_paths::resolve(&global_paths);

        // Simulate a crash: bind, then drop without cleanup, leaving the
        // socket file (and, separately, the lock file) behind.
        {
            let _crashed = Server::bind(&global_paths)
                .await
                .expect("first bind should succeed");
            // Dropped here without calling cleanup().
        }

        let mut recovered = Server::bind(&global_paths)
            .await
            .expect("stale artifacts must be recovered, not treated as a live daemon");
        let serve_task = tokio::spawn(async move {
            let _ = recovered.serve().await;
        });

        let mut connection = client::connect(&endpoint)
            .await
            .expect("connect after recovery should succeed");
        client::handshake(&mut connection, "test-client")
            .await
            .expect("handshake after recovery should succeed");

        serve_task.abort();
    }

    #[tokio::test]
    async fn concurrent_clients_can_all_handshake_and_query_status() {
        let home = TestHome::create("concurrent");
        let global_paths = home.global_paths();
        let mut server = Server::bind(&global_paths)
            .await
            .expect("bind should succeed");
        let endpoint = runtime_paths::resolve(&global_paths);
        let serve_task = tokio::spawn(async move {
            let _ = server.serve().await;
        });

        let mut client_tasks = Vec::new();
        for index in 0..8 {
            let endpoint = endpoint.clone();
            client_tasks.push(tokio::spawn(async move {
                let mut connection = client::connect(&endpoint)
                    .await
                    .unwrap_or_else(|error| panic!("client {index} connect failed: {error}"));
                client::handshake(&mut connection, "test-client")
                    .await
                    .unwrap_or_else(|error| panic!("client {index} handshake failed: {error}"));
                client::status(&mut connection)
                    .await
                    .unwrap_or_else(|error| panic!("client {index} status failed: {error}"));
            }));
        }

        for task in client_tasks {
            task.await.expect("client task should not panic");
        }

        serve_task.abort();
    }

    #[tokio::test]
    async fn cleanup_removes_the_runtime_artifacts() {
        let home = TestHome::create("cleanup");
        let global_paths = home.global_paths();
        let server = Server::bind(&global_paths)
            .await
            .expect("bind should succeed");
        let endpoint = runtime_paths::resolve(&global_paths);

        server.cleanup();

        #[cfg(unix)]
        assert!(!endpoint.socket_path.exists());
        assert!(!endpoint.lock_path.exists());
    }

    #[tokio::test]
    async fn install_via_daemon_creates_global_config_and_db_and_is_idempotent() {
        let home = TestHome::create("install");
        let global_paths = home.global_paths();
        let mut server = Server::bind(&global_paths)
            .await
            .expect("bind should succeed");
        let endpoint = runtime_paths::resolve(&global_paths);
        let serve_task = tokio::spawn(async move {
            let _ = server.serve().await;
        });

        let mut connection = client::connect(&endpoint)
            .await
            .expect("connect should succeed");
        client::handshake(&mut connection, "test-client")
            .await
            .expect("handshake should succeed");

        let first = send(
            &mut connection,
            Request::Install(brainprint_core::protocol::InstallRequest),
        )
        .await;
        let Response::Install(first_install) = first else {
            panic!("expected Install response, got {first:?}")
        };
        assert!(first_install.config_freshly_created);
        assert!(first_install.db_freshly_created);
        assert!(global_paths.config_file.is_file());
        assert!(global_paths.global_db.is_file());

        let second = send(
            &mut connection,
            Request::Install(brainprint_core::protocol::InstallRequest),
        )
        .await;
        let Response::Install(second_install) = second else {
            panic!("expected Install response, got {second:?}")
        };
        assert!(!second_install.config_freshly_created);
        assert!(!second_install.db_freshly_created);

        serve_task.abort();
    }

    #[tokio::test]
    async fn init_via_daemon_fresh_non_git_succeeds() {
        let home = TestHome::create("init-fresh");
        let global_paths = home.global_paths();
        let workspace = TestHome::create("init-fresh-workspace");
        let mut server = Server::bind(&global_paths)
            .await
            .expect("bind should succeed");
        let endpoint = runtime_paths::resolve(&global_paths);
        let serve_task = tokio::spawn(async move {
            let _ = server.serve().await;
        });

        let mut connection = client::connect(&endpoint)
            .await
            .expect("connect should succeed");
        client::handshake(&mut connection, "test-client")
            .await
            .expect("handshake should succeed");

        let response = send(
            &mut connection,
            Request::Init(brainprint_core::protocol::InitRequest {
                path: workspace.0.to_string_lossy().into_owned(),
            }),
        )
        .await;
        let Response::Init(init) = response else {
            panic!("expected Init response, got {response:?}")
        };
        assert!(init.freshly_created);
        assert!(!init.is_git);

        serve_task.abort();
    }

    #[tokio::test]
    async fn init_via_daemon_reinit_preserves_identity() {
        let home = TestHome::create("init-reinit");
        let global_paths = home.global_paths();
        let workspace = TestHome::create("init-reinit-workspace");
        let mut server = Server::bind(&global_paths)
            .await
            .expect("bind should succeed");
        let endpoint = runtime_paths::resolve(&global_paths);
        let serve_task = tokio::spawn(async move {
            let _ = server.serve().await;
        });

        let mut connection = client::connect(&endpoint)
            .await
            .expect("connect should succeed");
        client::handshake(&mut connection, "test-client")
            .await
            .expect("handshake should succeed");

        let request = Request::Init(brainprint_core::protocol::InitRequest {
            path: workspace.0.to_string_lossy().into_owned(),
        });
        let Response::Init(first) = send(&mut connection, request.clone()).await else {
            panic!("expected Init response")
        };
        assert!(first.freshly_created);

        let Response::Init(second) = send(&mut connection, request).await else {
            panic!("expected Init response")
        };
        assert!(!second.freshly_created);
        assert_eq!(second.project_id, first.project_id);
        assert_eq!(second.workspace_id, first.workspace_id);

        serve_task.abort();
    }

    #[tokio::test]
    async fn init_via_daemon_secondary_worktree_shares_project_id_with_new_workspace_id() {
        let home = TestHome::create("init-worktree");
        let global_paths = home.global_paths();
        let main_repo = TestHome::create("init-worktree-main");
        real_git_repo(&main_repo.0);
        let mut server = Server::bind(&global_paths)
            .await
            .expect("bind should succeed");
        let endpoint = runtime_paths::resolve(&global_paths);
        let serve_task = tokio::spawn(async move {
            let _ = server.serve().await;
        });

        let mut connection = client::connect(&endpoint)
            .await
            .expect("connect should succeed");
        client::handshake(&mut connection, "test-client")
            .await
            .expect("handshake should succeed");

        let main_response = send(
            &mut connection,
            Request::Init(brainprint_core::protocol::InitRequest {
                path: main_repo.0.to_string_lossy().into_owned(),
            }),
        )
        .await;
        let Response::Init(main_init) = main_response else {
            panic!("expected Init response for main repo, got {main_response:?}")
        };
        assert!(main_init.is_git);

        let secondary = TestHome::create("init-worktree-secondary");
        add_worktree(&main_repo.0, &secondary.0, "feature-x");

        let secondary_response = send(
            &mut connection,
            Request::Init(brainprint_core::protocol::InitRequest {
                path: secondary.0.to_string_lossy().into_owned(),
            }),
        )
        .await;
        let Response::Init(secondary_init) = secondary_response else {
            panic!("expected Init response for secondary worktree, got {secondary_response:?}")
        };

        assert_eq!(secondary_init.project_id, main_init.project_id);
        assert_ne!(secondary_init.workspace_id, main_init.workspace_id);

        let secondary_paths = brainprint_engine::paths::WorkspacePaths::from_root(
            std::path::PathBuf::from(&secondary_init.workspace_root),
        );
        assert!(!secondary_paths.project_db.exists());
        assert!(secondary_paths.workspace_db.is_file());

        serve_task.abort();
    }

    #[tokio::test]
    async fn init_identity_conflict_via_daemon_is_reported_as_conflict_error() {
        let home = TestHome::create("init-conflict");
        let global_paths = home.global_paths();
        let workspace = TestHome::create("init-conflict-workspace");
        fs::create_dir_all(workspace.0.join(".git")).expect(".git fixture should be created");
        let mut server = Server::bind(&global_paths)
            .await
            .expect("bind should succeed");
        let endpoint = runtime_paths::resolve(&global_paths);
        let serve_task = tokio::spawn(async move {
            let _ = server.serve().await;
        });

        let mut connection = client::connect(&endpoint)
            .await
            .expect("connect should succeed");
        client::handshake(&mut connection, "test-client")
            .await
            .expect("handshake should succeed");

        let first = send(
            &mut connection,
            Request::Init(brainprint_core::protocol::InitRequest {
                path: workspace.0.to_string_lossy().into_owned(),
            }),
        )
        .await;
        assert!(matches!(first, Response::Init(_)));

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

        let second = send(
            &mut connection,
            Request::Init(brainprint_core::protocol::InitRequest {
                path: workspace.0.to_string_lossy().into_owned(),
            }),
        )
        .await;
        let Response::Error(error) = second else {
            panic!("expected an Error response for the identity mismatch, got {second:?}")
        };
        assert_eq!(error.kind, brainprint_core::protocol::ErrorKind::Conflict);

        serve_task.abort();
    }
}
