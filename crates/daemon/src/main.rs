use brainprint_core::{BuildInfo, lifecycle::DETACHED_ARG};
use brainprint_daemon::server::Server;
use brainprint_engine::paths::GlobalPaths;

const HELP: &str = "Brainprint daemon

Usage:
  brainprintd
  brainprintd --help
  brainprintd --version

With no arguments, runs the daemon in the foreground until Ctrl+C.
`brainprint daemon start` (and any command that needs the daemon) runs it
in the background instead; `brainprint daemon stop` stops it.
";

#[tokio::main]
async fn main() {
    match std::env::args().nth(1).as_deref() {
        None => run(false).await,
        Some(DETACHED_ARG) => run(true).await,
        Some("-h" | "--help") => println!("{HELP}"),
        Some("-V" | "--version") => {
            let build = BuildInfo::current();
            println!("brainprintd {}", build.version);
        }
        Some(other) => {
            eprintln!("unknown argument: {other}\n\n{HELP}");
            std::process::exit(2);
        }
    }
}

/// `detached`: started in the background by `brainprint` (#89), so a
/// hangup from whatever terminal started it is not a reason to stop.
async fn run(detached: bool) {
    #[cfg(unix)]
    let _hangup = detached.then(|| {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .map_err(|error| eprintln!("brainprintd: cannot ignore SIGHUP: {error}"))
    });
    #[cfg(not(unix))]
    let _ = detached;

    let global_paths = match GlobalPaths::discover() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("brainprintd: {error}");
            std::process::exit(1);
        }
    };

    let mut server = match Server::bind(&global_paths).await {
        Ok(server) => server,
        Err(error) => {
            eprintln!("brainprintd: {error}");
            std::process::exit(1);
        }
    };

    eprintln!(
        "brainprintd {}: listening (pid {})",
        BuildInfo::current().version,
        std::process::id()
    );

    let stop = server.stop_requested();
    tokio::select! {
        result = server.serve() => {
            if let Err(error) = result {
                eprintln!("brainprintd: server error: {error}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            eprintln!("brainprintd: shutting down");
        }
        () = terminated() => {
            eprintln!("brainprintd: terminated, shutting down");
        }
        () = stop.notified() => {
            eprintln!("brainprintd: stop requested, shutting down");
        }
    }

    server.shutdown().await;
}

/// SIGTERM (logout, system shutdown) gets the same clean shutdown as
/// Ctrl+C. Never resolves where there is no such signal.
async fn terminated() {
    #[cfg(unix)]
    if let Ok(mut signal) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        signal.recv().await;
        return;
    }
    std::future::pending::<()>().await;
}
