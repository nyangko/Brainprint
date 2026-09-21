use brainprint_core::BuildInfo;
use brainprint_daemon::server::Server;
use brainprint_engine::paths::GlobalPaths;

const HELP: &str = "Brainprint daemon

Usage:
  brainprintd
  brainprintd --help
  brainprintd --version

With no arguments, runs the daemon in the foreground until Ctrl+C.
";

#[tokio::main]
async fn main() {
    match std::env::args().nth(1).as_deref() {
        None => run_foreground().await,
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

async fn run_foreground() {
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

    eprintln!("brainprintd: listening (pid {})", std::process::id());

    tokio::select! {
        result = server.serve() => {
            if let Err(error) = result {
                eprintln!("brainprintd: server error: {error}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            eprintln!("brainprintd: shutting down");
        }
    }

    server.cleanup();
}
