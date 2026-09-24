//! Shared per-tool-call flow: workspace/correlation resolution -> build
//! the wire operation -> connect -> `Query` -> `QueryAck` -> envelope.
//! Every `brainprint.*` tool mode (except `context status`, which has no
//! `Query`/ack step) goes through exactly this path, so the ack timing
//! rule (#25 "QueryAck": never before the response is received/decoded)
//! and the three error layers (#25 "Error mapping") are enforced once,
//! not once per tool.

use std::path::Path;

use brainprint_core::protocol::{
    EndpointPaths,
    query::{QueryOperationWire, QueryOutcomeWire},
};
use rmcp::{ErrorData, model::CallToolResult};

use crate::{
    daemon,
    envelope::{self, Outcome},
    params::{CorrelationParams, ParamError, WorkspaceSelectorParam},
};

pub fn invalid_params(error: ParamError) -> ErrorData {
    envelope::invalid_params(error.0)
}

/// `build_operation` runs only after the workspace/correlation params
/// themselves validate, so a bad target/budget never causes a wasted
/// daemon round trip.
pub async fn run_query(
    tool: &str,
    mode: &str,
    startup_cwd: &Path,
    endpoint_override: Option<&EndpointPaths>,
    workspace: WorkspaceSelectorParam,
    correlation: CorrelationParams,
    build_operation: impl FnOnce() -> Result<QueryOperationWire, ParamError>,
) -> Result<CallToolResult, ErrorData> {
    let workspace_wire = workspace.into_wire(startup_cwd).map_err(invalid_params)?;
    let correlation_wire = correlation.into_wire();
    let operation = build_operation().map_err(invalid_params)?;

    let mut connection = match daemon::connect_and_handshake(endpoint_override).await {
        Ok(connection) => connection,
        Err(error) => return Ok(envelope::transport_error(tool, mode, error)),
    };

    let request_id = uuid::Uuid::new_v4().to_string();
    let response = match daemon::query(
        &mut connection,
        request_id,
        workspace_wire,
        correlation_wire,
        operation,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => return Ok(envelope::transport_error(tool, mode, error)),
    };

    // #25 "QueryAck": sent only after the response was successfully
    // received and decoded, which it was by this point. A failed ack is
    // diagnostic only (stderr) -- it never invalidates the answer the
    // Agent already has.
    if let (QueryOutcomeWire::Ok(_), Some(ack_token)) =
        (&response.outcome, response.ack_token.clone())
    {
        let ack_request_id = uuid::Uuid::new_v4().to_string();
        if let Err(error) = daemon::ack(
            &mut connection,
            ack_request_id,
            response.workspace_id,
            ack_token,
        )
        .await
        {
            eprintln!("brainprint-mcp: QueryAck failed for {tool}/{mode}: {error}");
        }
    }

    Ok(match response.outcome {
        QueryOutcomeWire::Ok(result) => envelope::build(tool, mode, Outcome::Ok, result),
        QueryOutcomeWire::Err(error) => {
            envelope::build(tool, mode, Outcome::BrainprintError, error)
        }
    })
}
