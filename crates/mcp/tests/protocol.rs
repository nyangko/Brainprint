//! #25 "MCP protocol tests": a real `rmcp` client against the real
//! `BrainprintMcp` server, over an in-memory duplex pipe -- proves
//! `initialize`/`tools/list`/`tools/call` framing with the official SDK,
//! not a hand-rolled stub. No `brainprint-engine`/`brainprint-daemon`
//! dependency is needed here: a daemon-unavailable `tools/call` is
//! itself one of #25's required acceptance cases (no panic, a typed
//! transport-error result).

use std::time::Duration;

use brainprint_core::protocol::EndpointPaths;
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
/// no panic." #59: the endpoint is derived from a fresh runtime root
/// nothing ever binds, never the ambient `EndpointPaths::resolve()` one
/// -- a live user `brainprintd` in the real HOME would occupy that, and
/// this test must neither depend on it nor connect to it.
#[tokio::test(flavor = "multi_thread")]
async fn tools_call_with_no_daemon_running_is_a_typed_failure_not_a_panic() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let unbound = EndpointPaths::from_runtime_root(
        std::env::temp_dir().join(format!("bp-mcp-no-daemon-{}", std::process::id())),
    );

    let server = tokio::spawn(async move {
        let running = BrainprintMcp::with_endpoint(std::env::temp_dir(), unbound)
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

/// #66: a protocol mismatch with a running daemon is a typed transport
/// error naming both versions and which side to restart -- never a
/// silent answer. A newer daemon means this long-lived MCP process is the
/// stale one; Brainprint only says so, it never restarts its host.
#[tokio::test(flavor = "multi_thread")]
async fn a_protocol_mismatch_names_the_stale_side_in_both_directions() {
    use brainprint_core::{
        PROTOCOL_VERSION,
        protocol::{HandshakeResponse, Listener, Request, Response, framing},
    };

    for (daemon_version, remedy) in [
        (
            PROTOCOL_VERSION + 1,
            "restart or reconnect the Brainprint MCP server",
        ),
        (PROTOCOL_VERSION - 1, "the running brainprintd is older"),
    ] {
        let endpoint = EndpointPaths::from_runtime_root(std::env::temp_dir().join(format!(
            "bp-mcp-mismatch-{}-{daemon_version}",
            std::process::id()
        )));
        #[cfg(unix)]
        let mut listener = {
            std::fs::create_dir_all(&endpoint.runtime_root).expect("runtime root");
            let _ = std::fs::remove_file(&endpoint.socket_path);
            Listener::bind(&endpoint.socket_path).expect("listener")
        };
        #[cfg(windows)]
        let mut listener = Listener::bind(&endpoint.pipe_name).expect("listener");
        let fake_daemon = tokio::spawn(async move {
            let mut connection = listener.accept().await.expect("accept");
            let Request::Handshake(handshake) = framing::read_message(&mut connection)
                .await
                .expect("handshake")
            else {
                panic!("the handshake comes first")
            };
            framing::write_message(
                &mut connection,
                &Response::Handshake(HandshakeResponse::VersionMismatch {
                    server_protocol_version: daemon_version,
                    client_protocol_version: handshake.protocol_version,
                }),
            )
            .await
            .expect("reply");
            // Anything after a refused handshake is never served.
            let after: std::io::Result<Request> = framing::read_message(&mut connection).await;
            after.is_ok()
        });

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let mcp_endpoint = endpoint.clone();
        let server = tokio::spawn(async move {
            let running = BrainprintMcp::with_endpoint(std::env::temp_dir(), mcp_endpoint)
                .serve(server_io)
                .await
                .expect("server should start");
            running.waiting().await.expect("clean shutdown");
        });
        let client = timeout(STEP_TIMEOUT, ().serve(client_io))
            .await
            .expect("initialize timed out")
            .expect("client should initialize");

        let mut call = CallToolRequestParams::default();
        call.name = "brainprint.find".into();
        call.arguments = serde_json::json!({ "mode": "files" }).as_object().cloned();
        let result = timeout(STEP_TIMEOUT, client.call_tool(call))
            .await
            .expect("tools/call timed out")
            .expect("a mismatch is a tool-level result, not an MCP protocol error");
        assert_eq!(result.is_error, Some(true), "{result:?}");
        let structured = result.structured_content.expect("structured");
        assert_eq!(structured["outcome"], "transport_error");
        let message = structured["payload"]["message"].as_str().expect("message");
        assert!(
            message.contains(&format!("brainprintd speaks {daemon_version}"))
                && message.contains(&format!("brainprint-mcp speaks {PROTOCOL_VERSION}"))
                && message.contains(remedy),
            "{message}"
        );

        client.cancel().await.expect("client should close cleanly");
        server.await.expect("server task should not panic");
        assert!(
            !fake_daemon.await.expect("fake daemon"),
            "no request follows a refused handshake"
        );
    }
}
