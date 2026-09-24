//! #25 "Schema/token economy": exact serialized `tools/list` byte
//! measurements for the real four-tool grouped surface, plus a
//! test-only comparison fixture for a mechanically split
//! one-tool-per-Task10-operation surface (find, inspect, relations,
//! impact, context, knowledge, structure -- the 7 operations
//! `CoreQuerySurface` exposes), never used to claim provider token
//! savings -- only exact byte counts.

use brainprint_mcp::BrainprintMcp;
use rmcp::{ServiceExt, model::PaginatedRequestParams};
use schemars::JsonSchema;
use serde::Deserialize;

#[tokio::test]
async fn grouped_four_tool_schema_bytes_vs_mechanically_split_seven_tool_schema_bytes() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        let running = BrainprintMcp::new(std::env::temp_dir())
            .serve(server_io)
            .await
            .expect("server should start");
        running
            .waiting()
            .await
            .expect("server should shut down cleanly");
    });
    let client = ().serve(client_io).await.expect("client should initialize");
    let tools = client
        .list_tools(None::<PaginatedRequestParams>)
        .await
        .expect("tools/list should succeed");
    assert_eq!(tools.tools.len(), 4, "#25 locks exactly four tools");

    let mut total_schema_bytes = 0usize;
    let mut total_description_bytes = 0usize;
    println!("=== grouped (real, 4 tools) ===");
    for tool in &tools.tools {
        let schema_bytes = serde_json::to_string(&*tool.input_schema)
            .expect("schema should serialize")
            .len();
        let description_bytes = tool.description.as_deref().unwrap_or("").len();
        total_schema_bytes += schema_bytes;
        total_description_bytes += description_bytes;
        println!(
            "{}: schema_bytes={schema_bytes} description_bytes={description_bytes}",
            tool.name
        );
    }
    println!(
        "tool_count=4 total_schema_bytes={total_schema_bytes} total_description_bytes={total_description_bytes}"
    );

    client.cancel().await.expect("client should close cleanly");
    server.await.expect("server task should not panic");

    // ------------------------------------------------ mechanically split

    macro_rules! split_schema_bytes {
        ($ty:ty) => {{
            let schema = schemars::schema_for!($ty);
            serde_json::to_string(&schema)
                .expect("schema should serialize")
                .len()
        }};
    }

    let split_bytes = [
        ("find", split_schema_bytes!(SplitFind)),
        ("inspect", split_schema_bytes!(SplitInspect)),
        ("relations", split_schema_bytes!(SplitRelations)),
        ("impact", split_schema_bytes!(SplitImpact)),
        ("context", split_schema_bytes!(SplitContext)),
        ("knowledge", split_schema_bytes!(SplitKnowledge)),
        ("structure", split_schema_bytes!(SplitStructure)),
    ];
    let split_total: usize = split_bytes.iter().map(|(_, bytes)| bytes).sum();
    println!("=== mechanically split (test-only fixture, 7 tools) ===");
    for (name, bytes) in split_bytes {
        println!("{name}: schema_bytes={bytes}");
    }
    println!("tool_count=7 total_schema_bytes={split_total}");

    println!(
        "grouped_total_schema_bytes={total_schema_bytes} split_total_schema_bytes={split_total}"
    );
    // No token-savings claim from these byte counts (#25): recorded for
    // the completion record only, no assertion on which is smaller --
    // per-tool overhead (JSON Schema boilerplate x7 vs x4) and shared
    // vs. duplicated field definitions both move the total in ways a
    // single inequality would misrepresent as "grouping saves bytes"
    // when the real product claim is schema *count* economy (4 vs 7+
    // tool definitions), not raw bytes.
}

// Minimal per-operation input shapes, covering the same param names the
// real grouped tools use but with plain `String`/`bool` field types
// (not the full target/delivery/change-kind nested vocabulary, or its
// per-field doc-comment descriptions) -- a deliberately lower-bound
// estimate of a mechanically split surface's schema bytes, not a
// full-fidelity reimplementation. The measured totals below are still
// exact for *this* fixture; only the fixture's own fidelity is
// approximate (recorded honestly in the #25 completion record).

#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)]
struct SplitFind {
    resource_path: Option<String>,
    resource_id: Option<String>,
    resource_basename: Option<String>,
    resource_prefix: Option<String>,
    directory: Option<String>,
    recursive: Option<bool>,
    pattern: Option<String>,
    regex: Option<bool>,
    workspace_path: Option<String>,
    workspace_id: Option<String>,
    budget_profile: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)]
struct SplitInspect {
    resource_path: Option<String>,
    resource_id: Option<String>,
    workspace_path: Option<String>,
    workspace_id: Option<String>,
    budget_profile: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)]
struct SplitRelations {
    resource_path: Option<String>,
    resource_id: Option<String>,
    direction: Option<String>,
    kinds: Option<Vec<String>>,
    workspace_path: Option<String>,
    workspace_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)]
struct SplitImpact {
    resource_path: Option<String>,
    resource_id: Option<String>,
    change_kind: Option<String>,
    workspace_path: Option<String>,
    workspace_id: Option<String>,
    budget_profile: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)]
struct SplitContext {
    resource_path: Option<String>,
    work_item: Option<String>,
    change_kind: Option<String>,
    workspace_path: Option<String>,
    workspace_id: Option<String>,
    budget_profile: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)]
struct SplitKnowledge {
    kind: String,
    work_item: Option<String>,
    statuses: Option<Vec<String>>,
    id: Option<String>,
    workspace_path: Option<String>,
    workspace_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)]
struct SplitStructure {
    grouping: Option<String>,
    workspace_path: Option<String>,
    workspace_id: Option<String>,
}
