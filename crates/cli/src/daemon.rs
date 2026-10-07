//! `brainprint daemon start|stop|restart|status` (#89): the explicit face
//! of `brainprint_core::lifecycle`, which the auto-start also uses.

use brainprint_core::{
    PROTOCOL_VERSION,
    lifecycle::{self, Launch, LifecycleError, Probe, Started, Stopped},
    protocol::{EndpointPaths, StatusResponse},
};

use crate::query::{DaemonCommand, Exit};

pub async fn run(command: DaemonCommand) -> i32 {
    let endpoint = match EndpointPaths::resolve() {
        Ok(endpoint) => endpoint,
        Err(error) => {
            eprintln!("brainprint: {error}");
            return Exit::DaemonOrProtocolFailure.into();
        }
    };
    let result = match command {
        DaemonCommand::Start => start(&endpoint).await,
        DaemonCommand::Stop => stop(&endpoint).await,
        DaemonCommand::Restart => match stop(&endpoint).await {
            Ok(()) => start(&endpoint).await,
            error => error,
        },
        DaemonCommand::Status => status(&endpoint).await,
    };
    match result {
        Ok(()) => Exit::Ok.into(),
        Err(error) => {
            eprintln!("brainprint: {error}");
            Exit::DaemonOrProtocolFailure.into()
        }
    }
}

async fn start(endpoint: &EndpointPaths) -> Result<(), LifecycleError> {
    let launch = Launch::current_install(endpoint)?;
    match lifecycle::start(endpoint, &launch).await? {
        Started::Spawned(status) => {
            println!("started: {}", describe(&status));
            println!("  log: {}", launch.log_path.display());
        }
        Started::AlreadyRunning(status) => println!("already running: {}", describe(&status)),
    }
    Ok(())
}

async fn stop(endpoint: &EndpointPaths) -> Result<(), LifecycleError> {
    match lifecycle::stop(endpoint).await? {
        Stopped::Stopped { pid } => println!("stopped: brainprintd (pid {pid})"),
        Stopped::NotRunning => println!("not running"),
    }
    Ok(())
}

/// Read-only: reports what answers the endpoint, never starts anything.
async fn status(endpoint: &EndpointPaths) -> Result<(), LifecycleError> {
    match lifecycle::probe(endpoint).await? {
        Probe::Running(status) => println!(
            "running: {}, up {}s",
            describe(&status),
            status.uptime_seconds
        ),
        Probe::NotRunning => println!("not running"),
        Probe::Incompatible {
            server_protocol_version,
        } => {
            println!(
                "running, incompatible: brainprintd speaks protocol {server_protocol_version}, \
                 this brainprint speaks {PROTOCOL_VERSION}"
            );
            return Err(LifecycleError::Incompatible {
                server_protocol_version,
            });
        }
    }
    if !lifecycle::autostart_enabled() {
        println!("  auto-start: off ({} is set)", lifecycle::NO_AUTOSTART_ENV);
    }
    Ok(())
}

fn describe(status: &StatusResponse) -> String {
    format!(
        "brainprintd {} (protocol {}, pid {})",
        status.daemon_version, status.protocol_version, status.pid
    )
}
