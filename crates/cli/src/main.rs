mod args;
mod client;

use args::{ArgError, Command};
use brainprint_core::BuildInfo;

const HELP: &str = "Brainprint CLI

Usage:
  brainprint --help
  brainprint --version
  brainprint install
  brainprint init [path]
  brainprint status
";

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args::parse(&args) {
        Ok(Command::Help) => println!("{HELP}"),
        Ok(Command::Version) => {
            let build = BuildInfo::current();
            println!("brainprint {}", build.version);
        }
        Ok(Command::Install) => run(cmd_install()).await,
        Ok(Command::Status) => run(cmd_status()).await,
        Ok(Command::Init { path }) => run(cmd_init(path)).await,
        Err(ArgError::Unknown(other)) => {
            eprintln!("unknown argument: {other}\n\n{HELP}");
            std::process::exit(2);
        }
        Err(ArgError::TooManyArguments) => {
            eprintln!("too many arguments\n\n{HELP}");
            std::process::exit(2);
        }
    }
}

async fn run<F>(command: F)
where
    F: std::future::Future<Output = Result<(), client::CliError>>,
{
    if let Err(error) = command.await {
        eprintln!("brainprint: {error}");
        std::process::exit(1);
    }
}

async fn cmd_install() -> Result<(), client::CliError> {
    let mut connection = client::connect_and_handshake().await?;
    let install = client::install(&mut connection).await?;

    if install.config_freshly_created || install.db_freshly_created {
        println!("installed");
    } else {
        println!("already installed");
    }
    println!("  config: {}", install.global_config_path);
    println!("  db:     {}", install.global_db_path);
    Ok(())
}

async fn cmd_status() -> Result<(), client::CliError> {
    let mut connection = client::connect_and_handshake().await?;
    let status = client::status(&mut connection).await?;

    println!(
        "brainprintd {} (protocol {})",
        status.daemon_version, status.protocol_version
    );
    println!("  pid:    {}", status.pid);
    println!("  uptime: {}s", status.uptime_seconds);
    Ok(())
}

async fn cmd_init(path: Option<String>) -> Result<(), client::CliError> {
    let requested = path.unwrap_or_else(|| ".".to_owned());
    // Resolved against *this* process's cwd: brainprintd's cwd is
    // unrelated, so a relative path must never cross the wire as-is.
    let absolute_path = std::env::current_dir()
        .map(|cwd| cwd.join(&requested))
        .unwrap_or_else(|_| std::path::PathBuf::from(&requested));

    let mut connection = client::connect_and_handshake().await?;
    let init = client::init(
        &mut connection,
        absolute_path.to_string_lossy().into_owned(),
    )
    .await?;

    let verb = if init.freshly_created {
        "initialized"
    } else {
        "already initialized"
    };
    println!("{verb}: {}", init.workspace_root);
    println!("  project:   {}", init.project_id);
    println!("  workspace: {}", init.workspace_id);
    println!("  git:       {}", init.is_git);
    Ok(())
}
