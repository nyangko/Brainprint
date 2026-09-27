//! #35 MCP schema-economy decision-gate fixtures.
//!
//! This test does not change the product MCP surface. It measures four
//! candidate contracts with exact serialized JSON-Schema byte counts:
//! A) current four-tool surface, B) reduced typed four-tool candidate,
//! C) full-vocabulary split typed operations, and D) a small dispatcher
//! plus lazily disclosed operation contracts.
//!
//! Bytes are bytes, not token estimates. No winner is asserted here.

#![allow(dead_code)]

use brainprint_mcp::{
    BrainprintMcp,
    params::{
        BudgetProfileParam, CorrelationParams, DeliveryParams, ResourceKindParam,
        ResourceLanguageParam, ResourceRoleParam, SearchBudgetProfileParam, TargetParam,
        WorkspaceSelectorParam,
    },
    tools::{
        context::{
            ContextMode, GroupingParam, LineageOfParam, SummaryResourceScopeParam,
            WorkItemStatusParam,
        },
        find::FindMode,
        relations::{
            ChangeKindParam, RelationDirectionParam, RelationKindParam, RelationsMode,
        },
    },
};
use rmcp::{ServiceExt, model::PaginatedRequestParams};
use schemars::JsonSchema;
use serde::Deserialize;

fn schema_bytes<T: JsonSchema>() -> usize {
    serde_json::to_string(&schemars::schema_for!(T))
        .expect("schema should serialize")
        .len()
}

fn sum_named(items: &[(&str, usize)]) -> usize {
    items.iter().map(|(_, bytes)| *bytes).sum()
}

#[tokio::test]
async fn issue_35_schema_economy_candidates_are_measured_without_picking_a_winner() {
    // ------------------------------------------------ A: real current surface
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
    assert_eq!(tools.tools.len(), 4, "#25 baseline is exactly four tools");

    let mut baseline = Vec::new();
    let mut baseline_description_bytes = 0usize;
    for tool in &tools.tools {
        let bytes = serde_json::to_string(&*tool.input_schema)
            .expect("schema should serialize")
            .len();
        baseline.push((tool.name.to_string(), bytes));
        baseline_description_bytes += tool.description.as_deref().unwrap_or("").len();
    }
    let baseline_total: usize = baseline.iter().map(|(_, bytes)| *bytes).sum();

    client.cancel().await.expect("client should close cleanly");
    server.await.expect("server task should not panic");

    // ------------------------------------------ B: reduced typed 4-tool fixture
    // This candidate intentionally tests two hypotheses together:
    // 1) continuation is opaque to the Agent, and
    // 2) correlation/session bookkeeping is injected by integration rather
    //    than exposed in the model-visible tool schema.
    // Workspace remains explicit so generic MCP-only routing still has a
    // fallback. This is a measurement fixture, not a product decision.
    let reduced = [
        ("brainprint.find", schema_bytes::<ReducedFind>()),
        ("brainprint.inspect", schema_bytes::<ReducedInspect>()),
        ("brainprint.relations", schema_bytes::<ReducedRelations>()),
        ("brainprint.context", schema_bytes::<ReducedContext>()),
    ];
    let reduced_total = sum_named(&reduced);

    // -------------------------------- C: full-vocabulary small typed operations
    // Unlike #25's historical 7-tool lower-bound fixture, these shapes reuse
    // the real Target/Delivery/Workspace/Correlation/change/grouping vocabularies.
    let split = [
        ("find_target", schema_bytes::<FindTarget>()),
        ("find_files", schema_bytes::<FindFiles>()),
        ("find_text", schema_bytes::<FindText>()),
        ("inspect", schema_bytes::<Inspect>()),
        ("relations_direct", schema_bytes::<RelationsDirect>()),
        ("impact", schema_bytes::<Impact>()),
        ("context_change", schema_bytes::<ContextChange>()),
        ("context_resume", schema_bytes::<ContextResume>()),
        ("rules", schema_bytes::<Rules>()),
        ("work_items", schema_bytes::<WorkItems>()),
        ("lineage", schema_bytes::<Lineage>()),
        ("handoffs", schema_bytes::<Handoffs>()),
        ("structure", schema_bytes::<Structure>()),
        ("status", schema_bytes::<Status>()),
    ];
    let split_total = sum_named(&split);

    // ------------------------------------- D: dispatcher + lazy typed contract
    let dispatcher_bytes = schema_bytes::<Dispatcher>();
    let lazy_contract_total = split_total;
    let lazy_single_contract_min = split
        .iter()
        .map(|(_, bytes)| *bytes)
        .min()
        .expect("split fixture is non-empty");
    let lazy_single_contract_max = split
        .iter()
        .map(|(_, bytes)| *bytes)
        .max()
        .expect("split fixture is non-empty");

    // ------------------------------------- isolated common-field contributions
    let current_delivery = schema_bytes::<DeliveryParams>();
    let opaque_delivery = schema_bytes::<OpaqueDeliveryParams>();
    let correlation = schema_bytes::<CorrelationParams>();
    let workspace = schema_bytes::<WorkspaceSelectorParam>();

    println!("=== #35 A: current real MCP surface ===");
    for (name, bytes) in &baseline {
        println!("{name}: schema_bytes={bytes}");
    }
    println!(
        "tool_count={} total_schema_bytes={baseline_total} total_description_bytes={baseline_description_bytes}",
        baseline.len()
    );

    println!("=== #35 B: reduced typed 4-tool fixture ===");
    for (name, bytes) in reduced {
        println!("{name}: schema_bytes={bytes}");
    }
    println!("tool_count=4 total_schema_bytes={reduced_total}");

    println!("=== #35 C: full-vocabulary split typed fixture ===");
    for (name, bytes) in split {
        println!("{name}: schema_bytes={bytes}");
    }
    println!("tool_count=14 total_schema_bytes={split_total}");

    println!("=== #35 D: dispatcher + lazy operation contract ===");
    println!("dispatcher_schema_bytes={dispatcher_bytes}");
    println!("all_operation_contract_bytes={lazy_contract_total}");
    println!(
        "single_operation_contract_bytes_min={lazy_single_contract_min} max={lazy_single_contract_max}"
    );
    println!(
        "single_lazy_use_total_min={} single_lazy_use_total_max={}",
        dispatcher_bytes + lazy_single_contract_min,
        dispatcher_bytes + lazy_single_contract_max
    );

    println!("=== #35 common vocabulary ===");
    println!("current_delivery_schema_bytes={current_delivery}");
    println!("opaque_delivery_schema_bytes={opaque_delivery}");
    println!("correlation_schema_bytes={correlation}");
    println!("workspace_schema_bytes={workspace}");

    // Decision gate: only factual invariants, never "candidate X must be smaller".
    assert!(baseline_total > 0);
    assert!(reduced_total > 0);
    assert!(split_total > 0);
    assert!(dispatcher_bytes > 0);
    assert!(current_delivery > 0);
    assert!(opaque_delivery > 0);
}

// ---------------------------------------------------------------- B fixture

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct OpaqueDeliveryParams {
    budget_profile: BudgetProfileParam,
    /// Overrides the profile's max-items axis.
    max_items: Option<usize>,
    /// Overrides the profile's max-bytes axis.
    max_bytes: Option<usize>,
    /// Stateless opaque continuation returned by Brainprint. The Agent echoes
    /// it verbatim and does not interpret its internal revision/fingerprint key.
    continuation: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReducedFind {
    mode: FindMode,
    #[serde(flatten)]
    target: TargetParam,
    directory: Option<String>,
    #[serde(default)]
    recursive: bool,
    path_prefix: Option<String>,
    role: Option<ResourceRoleParam>,
    language: Option<ResourceLanguageParam>,
    kind: Option<ResourceKindParam>,
    limit: Option<usize>,
    pattern: Option<String>,
    #[serde(default)]
    regex: bool,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default = "default_true")]
    with_preview: bool,
    #[serde(default)]
    search_budget_profile: SearchBudgetProfileParam,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    delivery: OpaqueDeliveryParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReducedInspect {
    #[serde(flatten)]
    target: TargetParam,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    delivery: OpaqueDeliveryParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReducedRelations {
    mode: RelationsMode,
    #[serde(flatten)]
    target: TargetParam,
    #[serde(default)]
    direction: RelationDirectionParam,
    #[serde(default)]
    kinds: Vec<RelationKindParam>,
    change: Option<ChangeKindParam>,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    delivery: OpaqueDeliveryParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReducedContext {
    mode: ContextMode,
    #[serde(flatten)]
    target: TargetParam,
    change: Option<ChangeKindParam>,
    work_item: Option<String>,
    statuses: Option<Vec<WorkItemStatusParam>>,
    limit: Option<usize>,
    lineage_of: Option<LineageOfParam>,
    id: Option<String>,
    #[serde(default)]
    grouping: GroupingParam,
    #[serde(default)]
    resource_scope: SummaryResourceScopeParam,
    #[serde(default)]
    relation_kinds: Vec<RelationKindParam>,
    #[serde(default = "default_true")]
    include_ungrouped: bool,
    #[serde(default)]
    include_cycles: bool,
    member_sample_limit: Option<usize>,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    delivery: OpaqueDeliveryParams,
}

// ---------------------------------------------------------------- C fixture

#[derive(Debug, Deserialize, JsonSchema)]
struct FindTarget {
    #[serde(flatten)]
    target: TargetParam,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    correlation: CorrelationParams,
    #[serde(flatten)]
    delivery: DeliveryParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FindFiles {
    directory: Option<String>,
    #[serde(default)]
    recursive: bool,
    path_prefix: Option<String>,
    role: Option<ResourceRoleParam>,
    language: Option<ResourceLanguageParam>,
    kind: Option<ResourceKindParam>,
    limit: Option<usize>,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    correlation: CorrelationParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FindText {
    pattern: String,
    #[serde(default)]
    regex: bool,
    #[serde(default)]
    case_insensitive: bool,
    path_prefix: Option<String>,
    #[serde(default = "default_true")]
    with_preview: bool,
    #[serde(default)]
    search_budget_profile: SearchBudgetProfileParam,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    correlation: CorrelationParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Inspect {
    #[serde(flatten)]
    target: TargetParam,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    correlation: CorrelationParams,
    #[serde(flatten)]
    delivery: DeliveryParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RelationsDirect {
    #[serde(flatten)]
    target: TargetParam,
    #[serde(default)]
    direction: RelationDirectionParam,
    #[serde(default)]
    kinds: Vec<RelationKindParam>,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    correlation: CorrelationParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Impact {
    #[serde(flatten)]
    target: TargetParam,
    change: ChangeKindParam,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    correlation: CorrelationParams,
    #[serde(flatten)]
    delivery: DeliveryParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ContextChange {
    #[serde(flatten)]
    target: TargetParam,
    change: Option<ChangeKindParam>,
    work_item: Option<String>,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    correlation: CorrelationParams,
    #[serde(flatten)]
    delivery: DeliveryParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ContextResume {
    #[serde(flatten)]
    target: TargetParam,
    work_item: String,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    correlation: CorrelationParams,
    #[serde(flatten)]
    delivery: DeliveryParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Rules {
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    correlation: CorrelationParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkItems {
    statuses: Vec<WorkItemStatusParam>,
    limit: Option<usize>,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    correlation: CorrelationParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Lineage {
    lineage_of: LineageOfParam,
    id: String,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    correlation: CorrelationParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Handoffs {
    work_item: String,
    limit: Option<usize>,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    correlation: CorrelationParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Structure {
    #[serde(default)]
    grouping: GroupingParam,
    #[serde(default)]
    resource_scope: SummaryResourceScopeParam,
    #[serde(default)]
    relation_kinds: Vec<RelationKindParam>,
    #[serde(default = "default_true")]
    include_ungrouped: bool,
    #[serde(default)]
    include_cycles: bool,
    member_sample_limit: Option<usize>,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    correlation: CorrelationParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Status {}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------- D fixture

#[derive(Debug, Deserialize, JsonSchema)]
struct Dispatcher {
    /// Selects one Brainprint operation. The matching typed contract is
    /// disclosed only when the client/runtime can do so lazily.
    operation: DispatcherOperation,
    /// Request payload validated against the selected operation contract.
    request: serde_json::Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum DispatcherOperation {
    FindTarget,
    FindFiles,
    FindText,
    Inspect,
    RelationsDirect,
    Impact,
    ContextChange,
    ContextResume,
    Rules,
    WorkItems,
    Lineage,
    Handoffs,
    Structure,
    Status,
}
