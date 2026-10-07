mod client;
mod daemon;
mod query;
mod surface;
mod tui;
mod web;

use clap::Parser as _;
use query::Cli;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let exit_code = match Cli::try_parse() {
        Ok(Cli::Install) => run(cmd_install()).await,
        Ok(Cli::Daemon { command }) => daemon::run(command).await,
        Ok(Cli::Init { path }) => run(cmd_init(path)).await,
        Ok(Cli::Tui { workspace, locale }) => tui::run(&workspace, locale).await,
        Ok(Cli::Web {
            workspace,
            port,
            locale,
        }) => web::run(&workspace, port, locale).await,
        Ok(
            cli @ (Cli::Find { .. }
            | Cli::Inspect(_)
            | Cli::Relations(_)
            | Cli::Impact(_)
            | Cli::Context { .. }
            | Cli::Knowledge { .. }
            | Cli::Structure(_)
            | Cli::Work { .. }
            | Cli::Artifact { .. }
            | Cli::Verification { .. }
            | Cli::Doctor { .. }
            | Cli::Rebuild { .. }
            | Cli::Sync { .. }
            | Cli::Uninit { .. }
            | Cli::Status { .. }),
        ) => query::run_query_command(cli).await.into(),
        Err(error) => {
            // clap prints its own usage/help text to stdout/stderr as
            // appropriate; --help/--version exit 0, a syntax error exits
            // 2 (#24 §22).
            let code = error.exit_code();
            error.print().ok();
            code
        }
    };
    std::process::exit(exit_code);
}

async fn run<F>(command: F) -> i32
where
    F: std::future::Future<Output = Result<(), client::CliError>>,
{
    if let Err(error) = command.await {
        eprintln!("brainprint: {error}");
        return 1;
    }
    0
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
