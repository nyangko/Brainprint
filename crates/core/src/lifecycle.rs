//! Daemon process lifecycle (#89): is this user's `brainprintd` answering,
//! start one in the background, stop it.
//!
//! The one shared implementation behind `brainprint daemon
//! start|stop|restart|status` and the on-demand auto-start of the CLI,
//! TUI/Web and `brainprint-mcp`. It decides nothing about truth: it only
//! talks to the daemon endpoint and spawns a process.
//!
//! - Singleton: a spawned daemon either takes the daemon's own lock or
//!   exits with "already running"; whichever daemon then answers the
//!   endpoint is the one. Nothing here runs a second copy beside a live one.
//! - Identity: a daemon is whatever answers the Brainprint handshake on
//!   this user's private endpoint. Stopping it is a request over that
//!   endpoint -- no PID is ever signalled or killed.
//! - Binary: only the `brainprintd` next to the running executable (the same
//!   installation), never one found on `PATH`.
//! - Compatibility: a running daemon of another protocol is reported, never
//!   replaced.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use crate::{
    PROTOCOL_VERSION,
    protocol::{
        ClientConnection, EndpointPaths, HandshakeRequest, HandshakeResponse, Request, Response,
        ShutdownRequest, StatusRequest, StatusResponse, endpoint, framing,
    },
};

/// Set to a non-empty value to turn off the implicit auto-start of the
/// CLI, TUI/Web and MCP (CI, benchmarks, development). An explicit
/// `brainprint daemon start` still starts the daemon.
pub const NO_AUTOSTART_ENV: &str = "BRAINPRINT_NO_AUTOSTART";

/// The argument `brainprintd` runs with when started in the background.
pub const DETACHED_ARG: &str = "--detached";

const CLIENT_KIND: &str = "brainprint-lifecycle";
const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// A stop waits for managed verification Jobs to be stored INTERRUPTED.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
/// A spawned daemon that exits without serving (it lost the start race to
/// a daemon that is itself shutting down) is replaced at most this often.
const MAX_SPAWNS: usize = 3;
/// The log is moved to `brainprintd.log.1` once it grows past this.
const LOG_ROTATE_BYTES: u64 = 4 * 1024 * 1024;

/// What answers this user's daemon endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Running(StatusResponse),
    /// A daemon answers but speaks another protocol.
    Incompatible {
        server_protocol_version: u32,
    },
    NotRunning,
}

/// The `brainprintd` to start and where its output goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub daemon_exe: PathBuf,
    pub log_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Started {
    /// The daemon this call spawned is now serving.
    Spawned(StatusResponse),
    /// A daemon was already serving (or another start won the race).
    AlreadyRunning(StatusResponse),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// The daemon with this pid answered the stop and has shut down.
    Stopped {
        pid: u32,
    },
    NotRunning,
}

#[derive(Debug)]
pub enum LifecycleError {
    Io(io::Error),
    /// The running executable could not be located.
    CurrentExe(io::Error),
    /// No `brainprintd` next to the running executable.
    DaemonBinaryNotFound(PathBuf),
    /// A daemon of another protocol owns the endpoint.
    Incompatible {
        server_protocol_version: u32,
    },
    /// A daemon from before protocol 15 refused `Shutdown`: it can only be
    /// stopped where it runs.
    StopUnsupported {
        server_protocol_version: u32,
    },
    /// The daemon was started but never answered the handshake.
    NotReady {
        log_path: PathBuf,
        detail: String,
    },
    /// The daemon answered the stop request but did not finish shutting
    /// down in time.
    StopIncomplete {
        pid: u32,
    },
    /// No daemon is running and [`NO_AUTOSTART_ENV`] forbids starting one.
    AutostartDisabled,
    /// The daemon sent a response that does not match the request.
    UnexpectedResponse,
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(formatter, "daemon lifecycle I/O failed: {source}"),
            Self::CurrentExe(source) => write!(
                formatter,
                "cannot locate this executable to find its brainprintd: {source}"
            ),
            Self::DaemonBinaryNotFound(path) => write!(
                formatter,
                "brainprintd is not installed next to this executable (expected {}) -- \
                 install brainprint, brainprintd and brainprint-mcp together",
                path.display()
            ),
            Self::Incompatible {
                server_protocol_version,
            } => {
                write!(
                    formatter,
                    "a brainprintd speaking protocol {server_protocol_version} is running for this \
                     user; this installation speaks {PROTOCOL_VERSION}. It is not replaced \
                     automatically: "
                )?;
                if *server_protocol_version < PROTOCOL_VERSION {
                    formatter.write_str(
                        "stop it with `brainprint daemon stop` (one older than protocol 15: \
                         Ctrl+C where it runs, or end its brainprintd process), then retry",
                    )
                } else {
                    formatter.write_str("this installation is older -- use the matching brainprint")
                }
            }
            Self::StopUnsupported {
                server_protocol_version,
            } => write!(
                formatter,
                "the running brainprintd speaks protocol {server_protocol_version}, older than \
                 `brainprint daemon stop`; it is not replaced automatically: stop it with Ctrl+C \
                 where it runs, or end its brainprintd process, then retry"
            ),
            Self::NotReady { log_path, detail } => write!(
                formatter,
                "brainprintd was started but did not become ready ({detail}); see {}",
                log_path.display()
            ),
            Self::StopIncomplete { pid } => write!(
                formatter,
                "brainprintd (pid {pid}) accepted the stop but has not shut down within {}s",
                STOP_TIMEOUT.as_secs()
            ),
            Self::AutostartDisabled => write!(
                formatter,
                "brainprintd is not running and auto-start is off ({NO_AUTOSTART_ENV} is set) -- \
                 run `brainprint daemon start`"
            ),
            Self::UnexpectedResponse => {
                formatter.write_str("brainprintd sent an unexpected response")
            }
        }
    }
}

impl std::error::Error for LifecycleError {}

impl From<io::Error> for LifecycleError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

impl Launch {
    /// The `brainprintd` next to the running executable, logging to
    /// [`log_path`].
    pub fn current_install(endpoint: &EndpointPaths) -> Result<Self, LifecycleError> {
        let exe = std::env::current_exe().map_err(LifecycleError::CurrentExe)?;
        let daemon_exe = sibling_daemon(&exe);
        if !daemon_exe.is_file() {
            return Err(LifecycleError::DaemonBinaryNotFound(daemon_exe));
        }
        Ok(Self {
            daemon_exe,
            log_path: log_path(endpoint),
        })
    }
}

/// Where a background daemon writes its output: `~/.brainprint/logs/
/// brainprintd.log`, or the runtime root without a home directory.
#[must_use]
pub fn log_path(endpoint: &EndpointPaths) -> PathBuf {
    endpoint::daemon_log_path().unwrap_or_else(|| endpoint.runtime_root.join("brainprintd.log"))
}

/// `<dir of exe>/brainprintd[.exe]`.
#[must_use]
pub fn sibling_daemon(exe: &Path) -> PathBuf {
    exe.with_file_name(format!("brainprintd{}", std::env::consts::EXE_SUFFIX))
}

/// Whether the implicit auto-start is allowed: [`NO_AUTOSTART_ENV`] unset
/// or empty.
#[must_use]
pub fn autostart_enabled() -> bool {
    std::env::var_os(NO_AUTOSTART_ENV).is_none_or(|value| value.is_empty())
}

/// What answers `endpoint` right now. Read-only: never starts anything.
pub async fn probe(endpoint: &EndpointPaths) -> io::Result<Probe> {
    let mut connection = match connect(endpoint).await {
        Ok(connection) => connection,
        Err(source) if not_running(&source) => return Ok(Probe::NotRunning),
        Err(source) => return Err(source),
    };
    match handshake(&mut connection).await? {
        Ok(()) => {}
        Err(server_protocol_version) => {
            return Ok(Probe::Incompatible {
                server_protocol_version,
            });
        }
    }
    match round_trip(&mut connection, &Request::Status(StatusRequest::default())).await? {
        Response::Status(status) => Ok(Probe::Running(status)),
        _ => Err(io::Error::other("unexpected response to Status")),
    }
}

/// Start `launch.daemon_exe` in the background unless a daemon already
/// answers; return once one answers the handshake. Idempotent.
pub async fn start(endpoint: &EndpointPaths, launch: &Launch) -> Result<Started, LifecycleError> {
    match probe(endpoint).await? {
        Probe::Running(status) => return Ok(Started::AlreadyRunning(status)),
        Probe::Incompatible {
            server_protocol_version,
        } => {
            return Err(LifecycleError::Incompatible {
                server_protocol_version,
            });
        }
        Probe::NotRunning => {}
    }

    let deadline = Instant::now() + READY_TIMEOUT;
    let mut child = spawn(endpoint, launch)?;
    let mut spawns = 1;
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        match probe(endpoint).await {
            Ok(Probe::Running(status)) => {
                let ours = status.pid == child.id();
                reap(child);
                return Ok(if ours {
                    Started::Spawned(status)
                } else {
                    Started::AlreadyRunning(status)
                });
            }
            Ok(Probe::Incompatible {
                server_protocol_version,
            }) => {
                reap(child);
                return Err(LifecycleError::Incompatible {
                    server_protocol_version,
                });
            }
            // Not answering yet, or mid-start/shutdown: keep polling.
            Ok(Probe::NotRunning) | Err(_) => {}
        }
        if let Some(exit) = child.try_wait()? {
            // It found another daemon's lock: that daemon either answers on
            // the next poll or was shutting down, and then this one retries.
            if spawns == MAX_SPAWNS {
                return Err(LifecycleError::NotReady {
                    log_path: launch.log_path.clone(),
                    detail: format!("brainprintd exited {exit}"),
                });
            }
            child = spawn(endpoint, launch)?;
            spawns += 1;
        }
        if Instant::now() >= deadline {
            reap(child);
            return Err(LifecycleError::NotReady {
                log_path: launch.log_path.clone(),
                detail: format!("no handshake within {}s", READY_TIMEOUT.as_secs()),
            });
        }
    }
}

/// Ask the daemon on `endpoint` to shut down, and wait until it has: the
/// endpoint no longer answers and its lock file is gone.
pub async fn stop(endpoint: &EndpointPaths) -> Result<Stopped, LifecycleError> {
    let mut connection = match connect(endpoint).await {
        Ok(connection) => connection,
        Err(source) if not_running(&source) => return Ok(Stopped::NotRunning),
        Err(source) => return Err(source.into()),
    };
    // A mismatched daemon still honours `Shutdown` (#89) -- one from before
    // protocol 15 closes the connection instead.
    let mismatch = handshake(&mut connection).await?.err();
    let pid = match round_trip(&mut connection, &Request::Shutdown(ShutdownRequest)).await {
        Ok(Response::Shutdown(response)) => response.pid,
        Ok(_) => return Err(LifecycleError::UnexpectedResponse),
        Err(source) => {
            return Err(match mismatch {
                Some(server_protocol_version) => LifecycleError::StopUnsupported {
                    server_protocol_version,
                },
                None => source.into(),
            });
        }
    };
    drop(connection);

    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        let gone =
            matches!(probe(endpoint).await, Ok(Probe::NotRunning)) && !endpoint.lock_path.exists();
        if gone {
            return Ok(Stopped::Stopped { pid });
        }
        if Instant::now() >= deadline {
            return Err(LifecycleError::StopIncomplete { pid });
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// For a client that just found no daemon on `endpoint`: start one from
/// this installation, unless [`NO_AUTOSTART_ENV`] forbids it.
pub async fn ensure_started(endpoint: &EndpointPaths) -> Result<(), LifecycleError> {
    if !autostart_enabled() {
        return Err(LifecycleError::AutostartDisabled);
    }
    start(endpoint, &Launch::current_install(endpoint)?)
        .await
        .map(|_| ())
}

fn spawn(endpoint: &EndpointPaths, launch: &Launch) -> io::Result<Child> {
    // The daemon tightens this directory's permissions itself when it binds.
    fs::create_dir_all(&endpoint.runtime_root)?;
    let log = open_log(&launch.log_path)?;
    let mut command = Command::new(&launch.daemon_exe);
    command
        .arg(DETACHED_ARG)
        .current_dir(&endpoint.runtime_root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    spawn_detached(&mut command)
}

/// Unix: its own process group, so neither Ctrl+C in the caller's terminal
/// nor the hangup sent to the caller's jobs when that terminal closes
/// reaches it (`brainprintd --detached` also ignores SIGHUP).
#[cfg(unix)]
fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0).spawn()
}

/// Windows: no console and its own process group, so closing the caller's
/// console window or Ctrl+C there does not reach it; out of the caller's
/// job object where the job allows that, so it outlives the caller. And it
/// inherits none of the caller's own stdio handles -- see
/// [`stop_inheriting_stdio`].
#[cfg(windows)]
fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    use std::os::windows::process::CommandExt as _;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const ERROR_ACCESS_DENIED: i32 = 5;

    stop_inheriting_stdio();
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
    match command.spawn() {
        // The caller's job forbids breakaway: still detached from the console.
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED) => command
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .spawn(),
        result => result,
    }
}

/// std spawns every Windows child with `bInheritHandles = TRUE` (the
/// opt-out, `CommandExt::inherit_handles`, is unstable), so the daemon would
/// inherit this process's own stdin/stdout/stderr when they are inheritable
/// -- as they are when this process was itself started with pipes: an MCP
/// client's stdio, a script capturing `brainprint`'s output. The daemon
/// would then hold those pipes open for its whole lifetime, and whoever
/// waits for this process's output to end would wait for the daemon (#89).
/// Marking them not inheritable keeps them out; the daemon still gets the
/// log and NUL handles std duplicates for it. Nothing else in this process
/// passes them on implicitly: std duplicates an inherited stdio for each
/// child it starts.
#[cfg(windows)]
#[allow(unsafe_code)]
fn stop_inheriting_stdio() {
    use std::os::windows::io::{AsRawHandle as _, RawHandle};

    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetHandleInformation(handle: RawHandle, mask: u32, flags: u32) -> i32;
    }

    for handle in [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ] {
        if handle.is_null() {
            continue;
        }
        // SAFETY: `handle` is this process's own standard handle, valid for
        // the process's lifetime; only its inherit flag changes. A handle
        // that does not support it (a console, an invalid one) just fails,
        // and the result is not needed.
        let _ = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
    }
}

/// Wait for a spawned daemon on a thread of its own, so a long-lived
/// caller (`brainprint-mcp`) never keeps an exited daemon as a zombie.
fn reap(mut child: Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

fn open_log(path: &Path) -> io::Result<fs::File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::metadata(path).is_ok_and(|metadata| metadata.len() > LOG_ROTATE_BYTES) {
        let _ = fs::rename(path, path.with_extension("log.1"));
    }
    fs::OpenOptions::new().create(true).append(true).open(path)
}

async fn connect(endpoint: &EndpointPaths) -> io::Result<ClientConnection> {
    #[cfg(unix)]
    return ClientConnection::connect(&endpoint.socket_path).await;
    #[cfg(windows)]
    return ClientConnection::connect(&endpoint.pipe_name).await;
}

fn not_running(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// `Ok(Ok(()))` on a matching protocol, `Ok(Err(server_protocol_version))`
/// on a mismatch.
async fn handshake(connection: &mut ClientConnection) -> io::Result<Result<(), u32>> {
    let request = Request::Handshake(HandshakeRequest {
        protocol_version: PROTOCOL_VERSION,
        client_kind: CLIENT_KIND.to_owned(),
    });
    match round_trip(connection, &request).await? {
        Response::Handshake(HandshakeResponse::Ok { .. }) => Ok(Ok(())),
        Response::Handshake(HandshakeResponse::VersionMismatch {
            server_protocol_version,
            ..
        }) => Ok(Err(server_protocol_version)),
        _ => Err(io::Error::other("unexpected response to Handshake")),
    }
}

async fn round_trip(connection: &mut ClientConnection, request: &Request) -> io::Result<Response> {
    framing::write_message(connection, request).await?;
    framing::read_message(connection).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_daemon_is_the_sibling_of_the_running_executable() {
        let exe = Path::new("/opt/brainprint/bin")
            .join(format!("brainprint-mcp{}", std::env::consts::EXE_SUFFIX));
        assert_eq!(
            sibling_daemon(&exe),
            Path::new("/opt/brainprint/bin")
                .join(format!("brainprintd{}", std::env::consts::EXE_SUFFIX))
        );
    }

    #[test]
    fn a_large_log_is_rotated_once_and_a_fresh_one_appended() {
        let dir =
            std::env::temp_dir().join(format!("brainprint-lifecycle-log-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let log = dir.join("logs").join("brainprintd.log");
        fs::create_dir_all(log.parent().expect("parent")).expect("dir");
        fs::write(
            &log,
            vec![b'x'; usize::try_from(LOG_ROTATE_BYTES).expect("fits") + 1],
        )
        .expect("big log");

        drop(open_log(&log).expect("open"));

        assert_eq!(fs::metadata(&log).expect("fresh").len(), 0);
        assert!(log.with_extension("log.1").is_file());
        let _ = fs::remove_dir_all(&dir);
    }
}
