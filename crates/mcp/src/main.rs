//! `brainprint-mcp` binary entrypoint. stdin/stdout carry MCP protocol
//! only; every diagnostic goes to stderr (#25 "Transport").

use brainprint_mcp::BrainprintMcp;
use rmcp::{ServiceExt, transport::stdio};

#[tokio::main]
async fn main() {
    let startup_cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("brainprint-mcp: could not read the startup working directory: {error}");
            std::process::exit(1);
        }
    };

    let service = BrainprintMcp::new(startup_cwd);
    let running = match service.serve(stdio()).await {
        Ok(running) => running,
        Err(error) => {
            eprintln!("brainprint-mcp: failed to start on stdio: {error}");
            std::process::exit(1);
        }
    };

    if let Err(error) = running.waiting().await {
        eprintln!("brainprint-mcp: stdio session ended with an error: {error}");
        std::process::exit(1);
    }
}
