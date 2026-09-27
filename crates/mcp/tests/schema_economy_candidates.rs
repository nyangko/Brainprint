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
        BudgetProfileParam, ContinuationBudgetParam, CorrelationParams, DeliveryContinuationParam,
        DeliveryKeyParam, DeliveryParams, ResourceKindParam, ResourceLanguageParam,
        ResourceRoleParam, SearchBudgetProfileParam, TargetParam, WorkspaceSelectorParam,
    },
    tools::{
        context::{
            ContextMode, GroupingParam, LineageOfParam, SummaryResourceScopeParam,
            WorkItemStatusParam,
        },
        find::FindMode,
        relations::{ChangeKindParam, RelationDirectionParam, RelationKindParam, RelationsMode},
    },
};
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        Implementation, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
        ReadResourceResult, ResourceContents, ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
};
use schemars::JsonSchema;
use serde::Deserialize;

fn schema_value<T: JsonSchema>() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(T)).expect("schema should serialize")
}

fn schema_bytes<T: JsonSchema>() -> usize {
    serde_json::to_string(&schema_value::<T>())
        .expect("schema should serialize")
        .len()
}

#[derive(Debug, Clone)]
struct ContractMeasure {
    name: String,
    description: String,
    schema: serde_json::Value,
    schema_bytes: usize,
    contract_bytes: usize,
}

fn measure_contract_value(
    name: impl Into<String>,
    description: impl Into<String>,
    schema: serde_json::Value,
) -> ContractMeasure {
    let name = name.into();
    let description = description.into();
    let schema_bytes = serde_json::to_string(&schema)
        .expect("schema should serialize")
        .len();
    let contract_bytes = serde_json::to_string(&serde_json::json!({
        "name": &name,
        "description": &description,
        "inputSchema": &schema,
    }))
    .expect("tool contract should serialize")
    .len();
    ContractMeasure {
        name,
        description,
        schema,
        schema_bytes,
        contract_bytes,
    }
}

fn measure_contract<T: JsonSchema>(name: &str, description: &str) -> ContractMeasure {
    measure_contract_value(name, description, schema_value::<T>())
}

fn sum_schema_bytes(items: &[ContractMeasure]) -> usize {
    items.iter().map(|item| item.schema_bytes).sum()
}

fn sum_contract_bytes(items: &[ContractMeasure]) -> usize {
    items.iter().map(|item| item.contract_bytes).sum()
}

fn print_contracts(label: &str, items: &[ContractMeasure]) {
    println!("=== {label} ===");
    for item in items {
        println!(
            "{}: schema_bytes={} description_bytes={} contract_bytes={}",
            item.name,
            item.schema_bytes,
            item.description.len(),
            item.contract_bytes
        );
    }
    println!(
        "tool_count={} total_schema_bytes={} total_description_bytes={} total_contract_bytes={}",
        items.len(),
        sum_schema_bytes(items),
        items
            .iter()
            .map(|item| item.description.len())
            .sum::<usize>(),
        sum_contract_bytes(items)
    );
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

    let baseline: Vec<_> = tools
        .tools
        .iter()
        .map(|tool| {
            measure_contract_value(
                tool.name.to_string(),
                tool.description.as_deref().unwrap_or(""),
                serde_json::Value::Object((*tool.input_schema).clone()),
            )
        })
        .collect();

    client.cancel().await.expect("client should close cleanly");
    server.await.expect("server task should not panic");

    let baseline_description = |name: &str| {
        baseline
            .iter()
            .find(|item| item.name == name)
            .map(|item| item.description.as_str())
            .expect("baseline description should exist")
    };

    // ------------------------------------------ B: reduced typed 4-tool fixture
    // This candidate measures a stateless opaque-continuation adapter plus
    // integration-injected correlation. The opaque string is proven below to
    // round-trip the current structured continuation losslessly; no MCP-side
    // cursor/session map is introduced.
    let reduced = vec![
        measure_contract::<ReducedFind>("brainprint.find", baseline_description("brainprint.find")),
        measure_contract::<ReducedInspect>(
            "brainprint.inspect",
            baseline_description("brainprint.inspect"),
        ),
        measure_contract::<ReducedRelations>(
            "brainprint.relations",
            baseline_description("brainprint.relations"),
        ),
        measure_contract::<ReducedContext>(
            "brainprint.context",
            baseline_description("brainprint.context"),
        ),
    ];

    let continuation = DeliveryContinuationParam {
        workspace_id: "00000000-0000-0000-0000-000000000001".to_owned(),
        index_incarnation_id: "00000000-0000-0000-0000-000000000002".to_owned(),
        workspace_revision: "rev-7".to_owned(),
        generation_no: 3,
        generation_basis_revision: "rev-6".to_owned(),
        request_fingerprint: "request-fingerprint".to_owned(),
        projection_fingerprint: "projection-fingerprint".to_owned(),
        budget: ContinuationBudgetParam {
            max_items: Some(16),
            max_bytes: Some(16 * 1024),
            max_tokens: None,
        },
        next: DeliveryKeyParam {
            tier: 2,
            depth: 4,
            identity: "next-key".to_owned(),
        },
    };
    let opaque = encode_opaque_continuation(&continuation);
    let decoded = decode_opaque_continuation(&opaque).expect("opaque continuation should decode");
    assert_eq!(
        serde_json::to_value(&decoded).expect("decoded continuation should serialize"),
        serde_json::to_value(&continuation).expect("source continuation should serialize"),
        "opaque adapter must preserve every current continuation field"
    );

    // -------------------------------- C: full-fidelity small typed operations
    // These fixtures reuse the real nested vocabularies and retain the
    // operation-specific field documentation that contributes to JSON Schema.
    // Tool descriptions are measured too, so 4-tool vs 14-tool comparison does
    // not silently give the split candidate free description bytes.
    let split = split_contracts();

    // The dispatcher still validates the selected operation through its typed
    // request shape after lazy contract lookup. This does not replace the
    // operation's deeper runtime semantic validation (for example target
    // exactly-one rules), which remains part of the real adapter.
    validate_dispatch(
        DispatcherOperation::FindText,
        serde_json::json!({ "pattern": "needle" }),
    )
    .expect("typed dispatcher request should validate");
    assert!(
        validate_dispatch(DispatcherOperation::FindText, serde_json::json!({})).is_err(),
        "missing required operation fields must not silently validate"
    );

    // ------------------------------------- D: dispatcher + lazy typed contract
    // Unlike the first fixture, this performs a real rmcp resources/read
    // round-trip against a test-only resource server. Whether a real host puts
    // the returned resource into model-visible context is a Phase 2/client fact,
    // not inferred from this local protocol test.
    let dispatcher = measure_contract::<Dispatcher>(
        "brainprint.execute",
        "Execute one Brainprint operation. Read the matching brainprint://contracts/{operation} resource before first use when the host exposes MCP resources.",
    );

    let (lazy_client_io, lazy_server_io) = tokio::io::duplex(64 * 1024);
    let lazy_server = tokio::spawn(async move {
        let running = LazyContractServer
            .serve(lazy_server_io)
            .await
            .expect("lazy-contract server should start");
        running
            .waiting()
            .await
            .expect("lazy-contract server should shut down cleanly");
    });
    let lazy_client =
        ().serve(lazy_client_io)
            .await
            .expect("lazy-contract client should initialize");

    let mut lazy_lookup = Vec::new();
    for contract in &split {
        let uri = contract_uri(&contract.name);
        let result = lazy_client
            .read_resource(ReadResourceRequestParams::new(uri.clone()))
            .await
            .expect("resources/read should succeed");
        let text = match result.contents.as_slice() {
            [ResourceContents::TextResourceContents { text, .. }] => text.clone(),
            _ => panic!("contract resource should contain exactly one text payload"),
        };
        let returned: serde_json::Value =
            serde_json::from_str(&text).expect("contract resource should be JSON");
        assert_eq!(returned["name"], contract.name);
        assert_eq!(returned["description"], contract.description);
        assert_eq!(returned["inputSchema"], contract.schema);

        let request_bytes = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "resources/read",
            "params": { "uri": uri },
        }))
        .expect("resource request should serialize")
        .len();
        let result_bytes = serde_json::to_string(&result)
            .expect("resource result should serialize")
            .len();
        lazy_lookup.push((contract.name.clone(), request_bytes, result_bytes));
    }

    lazy_client
        .cancel()
        .await
        .expect("lazy-contract client should close cleanly");
    lazy_server
        .await
        .expect("lazy-contract server task should not panic");

    // ------------------------------------- isolated common-field contributions
    let current_delivery = schema_bytes::<DeliveryParams>();
    let opaque_delivery = schema_bytes::<OpaqueDeliveryParams>();
    let correlation = schema_bytes::<CorrelationParams>();
    let workspace = schema_bytes::<WorkspaceSelectorParam>();

    print_contracts("#35 A: current real MCP surface", &baseline);
    print_contracts("#35 B: reduced typed 4-tool fixture", &reduced);
    print_contracts("#35 C: full-fidelity split typed fixture", &split);

    println!("=== #35 D: dispatcher + real resources/read contract lookup ===");
    println!(
        "dispatcher_schema_bytes={} dispatcher_description_bytes={} dispatcher_contract_bytes={}",
        dispatcher.schema_bytes,
        dispatcher.description.len(),
        dispatcher.contract_bytes
    );
    for (name, request_bytes, result_bytes) in &lazy_lookup {
        println!(
            "{name}: resource_read_request_bytes={request_bytes} resource_read_result_bytes={result_bytes} first_use_bytes={}",
            request_bytes + result_bytes
        );
    }
    let lazy_min = lazy_lookup
        .iter()
        .map(|(_, request, result)| request + result)
        .min()
        .expect("lazy lookup fixture is non-empty");
    let lazy_max = lazy_lookup
        .iter()
        .map(|(_, request, result)| request + result)
        .max()
        .expect("lazy lookup fixture is non-empty");
    println!(
        "single_lazy_first_use_total_min={} single_lazy_first_use_total_max={}",
        dispatcher.contract_bytes + lazy_min,
        dispatcher.contract_bytes + lazy_max
    );

    println!("=== #35 common vocabulary ===");
    println!("current_delivery_schema_bytes={current_delivery}");
    println!("opaque_delivery_schema_bytes={opaque_delivery}");
    println!("correlation_schema_bytes={correlation}");
    println!("workspace_schema_bytes={workspace}");
    println!("opaque_continuation_payload_bytes={}", opaque.len());

    // Decision gate: factual invariants only, never "candidate X must be smaller".
    assert_eq!(sum_schema_bytes(&baseline), 24_069, "#25 baseline drifted");
    assert!(sum_schema_bytes(&reduced) > 0);
    assert!(sum_schema_bytes(&split) > 0);
    assert!(dispatcher.schema_bytes > 0);
    assert!(current_delivery > 0);
    assert!(opaque_delivery > 0);
}

// ---------------------------------------------------------------- B fixture

fn encode_opaque_continuation(value: &DeliveryContinuationParam) -> String {
    serde_json::to_string(value).expect("continuation should serialize")
}

fn decode_opaque_continuation(value: &str) -> Result<DeliveryContinuationParam, serde_json::Error> {
    serde_json::from_str(value)
}

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
    /// `mode: target` selector. Ignored for `files`/`text`.
    #[serde(flatten)]
    target: TargetParam,
    // `mode: files` fields.
    directory: Option<String>,
    #[serde(default)]
    recursive: bool,
    /// `files`: a Resource path prefix filter. `text`: a search scope
    /// prefix. Same field, same meaning, whichever mode reads it.
    path_prefix: Option<String>,
    role: Option<ResourceRoleParam>,
    language: Option<ResourceLanguageParam>,
    kind: Option<ResourceKindParam>,
    /// `files`: defaults to 100. Ignored for other modes.
    limit: Option<usize>,
    // `mode: text` fields.
    pattern: Option<String>,
    /// `pattern` is a regular expression rather than a literal string
    /// when `true`.
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
    /// `mode: target` delivery budget. Ignored for `files`/`text`.
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
    /// Empty means every kind. `mode: direct` only.
    #[serde(default)]
    kinds: Vec<RelationKindParam>,
    /// `mode: impact` only, required for that mode.
    change: Option<ChangeKindParam>,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    /// `mode: impact` delivery budget. Ignored for `direct`.
    #[serde(flatten)]
    delivery: OpaqueDeliveryParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReducedContext {
    mode: ContextMode,
    #[serde(flatten)]
    target: TargetParam,
    /// `mode: change` only. Omit for a plain edit with no declared
    /// change form.
    change: Option<ChangeKindParam>,
    /// Canonical UUID string. `mode: resume`/`handoffs`: required.
    /// `mode: change`: optional.
    work_item: Option<String>,
    /// `mode: work_items`: required.
    statuses: Option<Vec<WorkItemStatusParam>>,
    /// `mode: work_items`: defaults to 50. `mode: handoffs`: defaults to 20.
    limit: Option<usize>,
    /// `mode: lineage`: required.
    lineage_of: Option<LineageOfParam>,
    /// `mode: lineage`: required canonical Policy/Decision UUID.
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
    /// `mode: change`/`resume` delivery budget. Ignored otherwise.
    #[serde(flatten)]
    delivery: OpaqueDeliveryParams,
}

// ---------------------------------------------------------------- C fixture

fn split_contracts() -> Vec<ContractMeasure> {
    vec![
        measure_contract::<FindTarget>(
            "brainprint.find_target",
            "Resolve one explicit project target and deliver its bounded current projection.",
        ),
        measure_contract::<FindFiles>(
            "brainprint.find_files",
            "List indexed project files/resources using explicit directory, prefix, role, language, kind, and limit filters.",
        ),
        measure_contract::<FindText>(
            "brainprint.find_text",
            "Run an explicit bounded literal or regex text search. Text search is never an automatic fallback from a structured miss.",
        ),
        measure_contract::<Inspect>(
            "brainprint.inspect",
            "The resolved target's exact current declaration source (or a typed SourceUnavailable reason) plus both directions of its direct relations.",
        ),
        measure_contract::<RelationsDirect>(
            "brainprint.relations_direct",
            "Return one anchor's confirmed direct relations, one hop, unpaged, with no source materialization.",
        ),
        measure_contract::<Impact>(
            "brainprint.impact",
            "Run the I3 impact traversal for an explicitly declared ChangeKind; the change form is never inferred.",
        ),
        measure_contract::<ContextChange>(
            "brainprint.context_change",
            "Build bounded change context for an explicit edit target and optional declared change/work-item identity.",
        ),
        measure_contract::<ContextResume>(
            "brainprint.context_resume",
            "Resume an explicit WorkItem, optionally scoped to one target, with bounded delivery and continuation.",
        ),
        measure_contract::<Rules>(
            "brainprint.rules",
            "Return current applicable Policy/rules without inventing role, persona, or authority.",
        ),
        measure_contract::<WorkItems>(
            "brainprint.work_items",
            "Return current WorkItems matching explicit statuses under a bounded item limit.",
        ),
        measure_contract::<Lineage>(
            "brainprint.lineage",
            "Return one-hop lineage for an explicit ProjectPolicy, UserPolicy, or Decision identity.",
        ),
        measure_contract::<Handoffs>(
            "brainprint.handoffs",
            "Return handoff history for an explicit WorkItem, newest first, under a bounded item limit.",
        ),
        measure_contract::<Structure>(
            "brainprint.structure",
            "Return a deterministic grouped structural summary using explicit grouping, resource scope, relation kinds, cycle, and sample controls.",
        ),
        measure_contract::<Status>(
            "brainprint.status",
            "Return the daemon's own Status without fabricating a Workspace health score.",
        ),
    ]
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FindTarget {
    /// Exact target selector. The shared TargetParam schema preserves the
    /// current explicit target vocabulary and target_json escape hatch.
    #[serde(flatten)]
    target: TargetParam,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    correlation: CorrelationParams,
    /// Planner-backed delivery budget and exact continuation.
    #[serde(flatten)]
    delivery: DeliveryParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FindFiles {
    /// Optional directory selector for indexed file/resource discovery.
    directory: Option<String>,
    #[serde(default)]
    recursive: bool,
    /// Resource path prefix filter.
    path_prefix: Option<String>,
    role: Option<ResourceRoleParam>,
    language: Option<ResourceLanguageParam>,
    kind: Option<ResourceKindParam>,
    /// Defaults to 100 in the current operation semantics.
    limit: Option<usize>,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    correlation: CorrelationParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FindText {
    /// Explicit literal or regex pattern. Required for text search.
    pattern: String,
    /// Treat pattern as a regular expression rather than a literal string.
    #[serde(default)]
    regex: bool,
    #[serde(default)]
    case_insensitive: bool,
    /// Search scope prefix.
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
    /// Empty means every relation kind.
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
    /// Required explicit change form; never inferred by MCP/Agent.
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
    /// Optional declared change form; omission means a plain edit.
    change: Option<ChangeKindParam>,
    /// Optional canonical WorkItem UUID this edit belongs to.
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
    /// Optional target: an empty selector is a valid unscoped Resume.
    #[serde(flatten)]
    target: TargetParam,
    /// Required canonical WorkItem UUID.
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
    /// Required explicit WorkItem statuses.
    statuses: Vec<WorkItemStatusParam>,
    /// Defaults to 50 in the current operation semantics.
    limit: Option<usize>,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    correlation: CorrelationParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Lineage {
    /// Required lineage target kind.
    lineage_of: LineageOfParam,
    /// Required canonical UUID of the Policy or Decision.
    id: String,
    #[serde(flatten)]
    workspace: WorkspaceSelectorParam,
    #[serde(flatten)]
    correlation: CorrelationParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Handoffs {
    /// Required canonical WorkItem UUID.
    work_item: String,
    /// Defaults to 20 in the current operation semantics.
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
    /// disclosed only when the client/runtime can expose MCP resources lazily.
    operation: DispatcherOperation,
    /// Request payload validated against the selected operation contract.
    request: serde_json::Value,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
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

fn contract_operation(name: &str) -> &str {
    name.strip_prefix("brainprint.").unwrap_or(name)
}

fn contract_uri(name: &str) -> String {
    format!("brainprint://contracts/{}", contract_operation(name))
}

fn contract_by_operation(operation: &str) -> Option<ContractMeasure> {
    split_contracts()
        .into_iter()
        .find(|item| contract_operation(&item.name) == operation)
}

fn validate_dispatch(
    operation: DispatcherOperation,
    request: serde_json::Value,
) -> Result<(), serde_json::Error> {
    macro_rules! validate {
        ($ty:ty) => {
            serde_json::from_value::<$ty>(request).map(|_| ())
        };
    }
    match operation {
        DispatcherOperation::FindTarget => validate!(FindTarget),
        DispatcherOperation::FindFiles => validate!(FindFiles),
        DispatcherOperation::FindText => validate!(FindText),
        DispatcherOperation::Inspect => validate!(Inspect),
        DispatcherOperation::RelationsDirect => validate!(RelationsDirect),
        DispatcherOperation::Impact => validate!(Impact),
        DispatcherOperation::ContextChange => validate!(ContextChange),
        DispatcherOperation::ContextResume => validate!(ContextResume),
        DispatcherOperation::Rules => validate!(Rules),
        DispatcherOperation::WorkItems => validate!(WorkItems),
        DispatcherOperation::Lineage => validate!(Lineage),
        DispatcherOperation::Handoffs => validate!(Handoffs),
        DispatcherOperation::Structure => validate!(Structure),
        DispatcherOperation::Status => validate!(Status),
    }
}

#[derive(Debug, Clone, Copy)]
struct LazyContractServer;

impl ServerHandler for LazyContractServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_resources().build())
            .with_server_info(Implementation::new(
                "brainprint-schema-economy-fixture",
                "0",
            ))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let prefix = "brainprint://contracts/";
        let Some(name) = request.uri.strip_prefix(prefix) else {
            return Err(ErrorData::resource_not_found(
                "unknown contract resource",
                Some(serde_json::json!({ "uri": request.uri })),
            ));
        };
        let Some(contract) = contract_by_operation(name) else {
            return Err(ErrorData::resource_not_found(
                "unknown contract resource",
                Some(serde_json::json!({ "uri": request.uri })),
            ));
        };
        let text = serde_json::to_string(&serde_json::json!({
            "name": contract.name,
            "description": contract.description,
            "inputSchema": contract.schema,
        }))
        .expect("contract resource should serialize");
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(text, request.uri).with_mime_type("application/schema+json"),
        ])
        .into())
    }
}
