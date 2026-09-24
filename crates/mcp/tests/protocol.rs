//! #25 "MCP protocol tests": a real `rmcp` client against the real
//! `BrainprintMcp` server, over an in-memory duplex pipe -- proves
//! `initialize`/`tools/list`/`tools/call` framing with the official SDK,
//! not a hand-rolled stub. No `brainprint-engine`/`brainprint-daemon`
//! dependency is needed here: a daemon-unavailable `tools/call` is
//! itself one of #25's required acceptance cases (no panic, a typed
//! transport-error result).

use std::time::Duration;

use brainprint_mcp::BrainprintMcp;
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, PaginatedRequestParams},
};
use tokio::time::timeout;

const STEP_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread")]
async fn initialize_and_tools_list_exposes_exactly_four_brainprint_tools() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);

    let server = tokio::spawn(async move {
        let running = BrainprintMcp::new(std::env::temp_dir())
            .serve(server_io)
            .await
            .expect("server should start on the duplex transport");
        running
            .waiting()
            .await
            .expect("server should shut down cleanly");
    });

    let client = timeout(STEP_TIMEOUT, ().serve(client_io))
        .await
        .expect("client initialize timed out")
        .expect("client should initialize");

    let tools = timeout(
        STEP_TIMEOUT,
        client.list_tools(None::<PaginatedRequestParams>),
    )
    .await
    .expect("tools/list timed out")
    .expect("tools/list should succeed");

    assert_eq!(
        tools.tools.len(),
        4,
        "expected exactly four Brainprint tools, got {:?}",
        tools
            .tools
            .iter()
            .map(|tool| &tool.name)
            .collect::<Vec<_>>()
    );
    let mut names: Vec<&str> = tools.tools.iter().map(|tool| tool.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "brainprint.context",
            "brainprint.find",
            "brainprint.inspect",
            "brainprint.relations",
        ],
        "the exact four tools #25 locks"
    );

    client.cancel().await.expect("client should close cleanly");
    server.await.expect("server task should not panic");
}

/// #25 acceptance 5: "daemon unavailable -> compact typed tool failure,
/// no panic." This test process sets no `brainprintd` socket/pipe up
/// anywhere, so `EndpointPaths::resolve()`'s real ambient endpoint is
/// unoccupied and every `tools/call` here exercises exactly that path.
#[tokio::test(flavor = "multi_thread")]
async fn tools_call_with_no_daemon_running_is_a_typed_failure_not_a_panic() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);

    let server = tokio::spawn(async move {
        let running = BrainprintMcp::new(std::env::temp_dir())
            .serve(server_io)
            .await
            .expect("server should start on the duplex transport");
        running
            .waiting()
            .await
            .expect("server should shut down cleanly");
    });

    let client = timeout(STEP_TIMEOUT, ().serve(client_io))
        .await
        .expect("client initialize timed out")
        .expect("client should initialize");

    let mut call = CallToolRequestParams::default();
    call.name = "brainprint.context".into();
    call.arguments = serde_json::json!({ "mode": "status" }).as_object().cloned();
    let result = timeout(STEP_TIMEOUT, client.call_tool(call))
        .await
        .expect("tools/call timed out")
        .expect("tools/call itself must not fail as an MCP protocol error");

    assert_eq!(
        result.is_error,
        Some(true),
        "a daemon-unavailable call is a tool-level error result, got {result:?}"
    );
    let structured = result
        .structured_content
        .expect("structured content must carry the typed transport error");
    assert_eq!(structured["outcome"], "transport_error");

    client.cancel().await.expect("client should close cleanly");
    server.await.expect("server task should not panic");
}
