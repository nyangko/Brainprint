//! Task 11 CLI: the seven Core query operation families plus their
//! shared budget/retention/continuation/output grammar (#24
//! "Implementation contract locked").

mod artifact;
mod base64url;
pub mod commands;
mod delivery;
mod exec;
mod knowledge;
mod maintenance;
mod render;
mod target;
mod verification;
mod vocab;
mod work;

pub use artifact::ArtifactCommand;
pub use commands::Cli;
pub use exec::Exit;
pub use verification::VerificationCommand;
pub use work::WorkCommand;

/// Dispatches every `Cli` variant except `Install`/`Status`/`Init`, which
/// `main.rs` still handles directly (unchanged Task 11 behavior other
/// than the protocol v2 handshake).
pub async fn run_query_command(cli: Cli) -> Exit {
    match cli {
        Cli::Find { mode } => exec::run_find(mode).await,
        Cli::Inspect(args) => exec::run_inspect(args).await,
        Cli::Relations(args) => exec::run_relations(args).await,
        Cli::Impact(args) => exec::run_impact(args).await,
        Cli::Context { mode } => exec::run_context(mode).await,
        Cli::Knowledge { mode } => exec::run_knowledge(mode).await,
        Cli::Structure(args) => exec::run_structure(args).await,
        Cli::Work { mode } => work::run_work(mode).await,
        Cli::Artifact { mode } => artifact::run_artifact(mode).await,
        Cli::Verification { mode } => verification::run_verification(mode).await,
        Cli::Doctor { path, json } => maintenance::run_doctor(path, json).await,
        Cli::Rebuild { path, json } => maintenance::run_rebuild(path, json).await,
        Cli::Install | Cli::Status | Cli::Init { .. } => {
            unreachable!("main.rs handles Install/Status/Init before dispatching here")
        }
    }
}
