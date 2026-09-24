//! The `brainprint-mcp` server: exactly four tools (#25 "Four-tool
//! surface"), each a thin `Parameters<...>` -> shared `execute::run_query`
//! -> `CallToolResult` mapping. No project truth is decided here.

use std::path::PathBuf;

use brainprint_core::protocol::EndpointPaths;
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};

use crate::{
    daemon,
    envelope::{self, Outcome},
    execute,
    tools::{
        context::ContextBuild, context::ContextParams, find::FindParams, inspect::InspectParams,
        relations::RelationsParams,
    },
};

const INSTRUCTIONS: &str = "Brainprint exposes indexed project truth (find/inspect/relations/context) \
through a thin adapter over the local brainprintd daemon. See the \
integrations/brainprint/SKILL.md instruction artifact for when to prefer \
it over Read/Grep/Glob. It never ranks, resolves ambiguity, or picks a \
Working State on its own -- every typed outcome (NOT_INITIALIZED, \
WORKSPACE_AMBIGUOUS, partial coverage, and the rest) is returned exactly \
as Task 10/11 produced it.";

pub struct BrainprintMcp {
    startup_cwd: PathBuf,
    /// `None` in every real (production) instance: the daemon endpoint
    /// resolves from the real ambient environment, identically to
    /// `brainprint-cli`. See `daemon::connect_and_handshake`'s doc for
    /// why this seam exists at all.
    endpoint_override: Option<EndpointPaths>,
}

#[tool_router]
impl BrainprintMcp {
    pub const fn new(startup_cwd: PathBuf) -> Self {
        Self {
            startup_cwd,
            endpoint_override: None,
        }
    }

    /// Test/advanced-only: point this server at an exact daemon endpoint
    /// instead of resolving one from the real ambient environment. Not
    /// used by the `brainprint-mcp` binary itself.
    #[must_use]
    pub const fn with_endpoint(startup_cwd: PathBuf, endpoint: EndpointPaths) -> Self {
        Self {
            startup_cwd,
            endpoint_override: Some(endpoint),
        }
    }

    /// find: target / files / explicit text (#25 "brainprint.find").
    #[tool(
        name = "brainprint.find",
        description = "Locate project facts by exact/search target, list files from the index, \
                        or explicit text search. `mode` selects target|files|text; text is never \
                        an automatic fallback from a structured miss."
    )]
    pub async fn find(
        &self,
        Parameters(params): Parameters<FindParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let built = params.split();
        execute::run_query(
            "brainprint.find",
            built.mode,
            &self.startup_cwd,
            self.endpoint_override.as_ref(),
            built.workspace,
            built.correlation,
            || built.operation,
        )
        .await
    }

    /// inspect: exact current source + direct relations of one resolved
    /// target (#25 "brainprint.inspect").
    #[tool(
        name = "brainprint.inspect",
        description = "The resolved target's exact current declaration source (or a typed \
                        SourceUnavailable reason) plus both directions of its direct relations."
    )]
    pub async fn inspect(
        &self,
        Parameters(params): Parameters<InspectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let built = params.split();
        execute::run_query(
            "brainprint.inspect",
            "inspect",
            &self.startup_cwd,
            self.endpoint_override.as_ref(),
            built.workspace,
            built.correlation,
            || built.operation,
        )
        .await
    }

    /// relations: direct / impact (#25 "brainprint.relations").
    #[tool(
        name = "brainprint.relations",
        description = "`direct`: one anchor's confirmed relations, one hop, unpaged, no source. \
                        `impact`: the I3 traversal for a declared ChangeKind (caller states the \
                        change form explicitly -- never inferred)."
    )]
    pub async fn relations(
        &self,
        Parameters(params): Parameters<RelationsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let built = params.split();
        execute::run_query(
            "brainprint.relations",
            built.mode,
            &self.startup_cwd,
            self.endpoint_override.as_ref(),
            built.workspace,
            built.correlation,
            || built.operation,
        )
        .await
    }

    /// context: change / resume / rules / work_items / lineage /
    /// handoffs / structure / status (#25 "brainprint.context").
    #[tool(
        name = "brainprint.context",
        description = "Grouped Task 10/11 context operations, selected by `mode`: change (an \
                        edit at a target), resume (an explicit WorkItem), rules (current \
                        applicable Policy), work_items (by status), lineage (one Policy/Decision's \
                        one-hop history), handoffs (a WorkItem's handoff history), structure \
                        (grouped structural summary), status (the daemon's own Status -- no \
                        fabricated Workspace health score)."
    )]
    pub async fn context(
        &self,
        Parameters(params): Parameters<ContextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match params.split() {
            ContextBuild::Query {
                workspace,
                correlation,
                mode,
                operation,
            } => {
                execute::run_query(
                    "brainprint.context",
                    mode,
                    &self.startup_cwd,
                    self.endpoint_override.as_ref(),
                    workspace,
                    correlation,
                    || operation,
                )
                .await
            }
            ContextBuild::Status => Ok(
                match daemon::connect_and_handshake(self.endpoint_override.as_ref()).await {
                    Ok(mut connection) => match daemon::status(&mut connection).await {
                        Ok(status) => {
                            envelope::build("brainprint.context", "status", Outcome::Ok, status)
                        }
                        Err(error) => {
                            envelope::transport_error("brainprint.context", "status", error)
                        }
                    },
                    Err(error) => envelope::transport_error("brainprint.context", "status", error),
                },
            ),
        }
    }
}

#[tool_handler]
impl ServerHandler for BrainprintMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "brainprint-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }
}
