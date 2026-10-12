//! Query command execution: build the wire request, send it, render the
//! response, and run the #24 §23 ACK/output lifecycle.

use std::num::NonZeroUsize;

use brainprint_core::{
    ResourceId, WorkItemId,
    protocol::{
        Request, Response,
        query::{
            AckStatusWire, DeliveryBudgetWire, DeliveryWire, FindQueryWire, FindResultWire,
            GraphEndpointWire, KnowledgeResultWire, KnowledgeWire, ProjectionTargetWire,
            QueryAckRequest, QueryErrorCodeWire, QueryOperationWire, QueryOutcomeWire,
            QueryRequest, QueryResultWire, ResourceTargetWire, RetentionWire,
            TargetResolutionWire, WorkItemStatusWire, WorkItemWire, WorkspaceSelectorWire,
        },
    },
};

use super::{
    commands::{
        CommonArgs, ContextChangeArgs, ContextCommand, ContextResumeArgs, FindCommand,
        FindFilesArgs, FindTargetArgs, FindTextArgs, ImpactArgs, InspectArgs, KnowledgeCommand,
        KnowledgeHandoffsArgs, KnowledgeLineageArgs, KnowledgeRulesArgs, KnowledgeWorkItemsArgs,
        RelationsArgs, StructureArgs,
    },
    delivery::DeliveryArgsError,
    knowledge::InvalidKnowledgeJson,
    render,
    target::{TargetArgs, TargetArgsError},
};
use crate::client;

/// #24 §22's exact exit-code table.
#[derive(Debug, Clone, Copy)]
pub enum Exit {
    Ok = 0,
    CliSyntax = 2,
    WorkspaceConflict = 3,
    QueryOrDeliveryFailure = 4,
    DaemonOrProtocolFailure = 5,
}

impl From<Exit> for i32 {
    fn from(value: Exit) -> Self {
        value as i32
    }
}

pub(super) fn exit_for_code(code: QueryErrorCodeWire) -> Exit {
    match code {
        QueryErrorCodeWire::InvalidRequest => Exit::CliSyntax,
        QueryErrorCodeWire::NotInitialized
        | QueryErrorCodeWire::WorkspaceRootMissing
        | QueryErrorCodeWire::WorkspaceAmbiguous
        | QueryErrorCodeWire::WorkspaceMismatch
        | QueryErrorCodeWire::WorkspaceBindingMismatch => Exit::WorkspaceConflict,
        QueryErrorCodeWire::BudgetTooSmall
        | QueryErrorCodeWire::ContinuationMismatch
        | QueryErrorCodeWire::InvalidDeliveryRequest
        | QueryErrorCodeWire::WorkItemNotFound
        | QueryErrorCodeWire::QueryFailed
        | QueryErrorCodeWire::ResultTooLarge => Exit::QueryOrDeliveryFailure,
        QueryErrorCodeWire::DaemonInternal => Exit::DaemonOrProtocolFailure,
    }
}

pub(crate) fn absolute_workspace(path: &str) -> String {
    std::path::Path::new(path)
        .canonicalize()
        .or_else(|_| std::env::current_dir().map(|cwd| cwd.join(path)))
        .map(|absolute| absolute.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_owned())
}

/// A CLI-side construction failure (bad flag combination, invalid JSON):
/// always exit 2, the request never leaves this process.
pub struct CliSyntaxError(pub String);

impl From<TargetArgsError> for CliSyntaxError {
    fn from(error: TargetArgsError) -> Self {
        Self(match error {
            TargetArgsError::NoSelector => "exactly one target selector is required".to_owned(),
            TargetArgsError::MultipleSelectors => {
                "at most one target selector may be given".to_owned()
            }
            TargetArgsError::SymbolFilterWithoutSymbolSelector => {
                "--in-resource/--symbol-kind/--language require a Symbol selector".to_owned()
            }
            TargetArgsError::InvalidTargetJson(error) => format!("invalid --target-json: {error}"),
        })
    }
}

impl From<DeliveryArgsError> for CliSyntaxError {
    fn from(error: DeliveryArgsError) -> Self {
        Self(match error {
            DeliveryArgsError::RetainedWithoutCorrelation => {
                "--retention retained requires --client-id or --session-id".to_owned()
            }
            DeliveryArgsError::InvalidContinuation => "invalid --continuation token".to_owned(),
        })
    }
}

impl From<InvalidKnowledgeJson> for CliSyntaxError {
    fn from(error: InvalidKnowledgeJson) -> Self {
        Self(format!("invalid knowledge JSON flag: {}", error.0))
    }
}

async fn run(common: &CommonArgs, operation: QueryOperationWire) -> Exit {
    let mut connection = match client::connect_and_handshake().await {
        Ok(connection) => connection,
        Err(error) => {
            eprintln!("brainprint: {error}");
            return Exit::DaemonOrProtocolFailure;
        }
    };

    let request_id = uuid::Uuid::new_v4().to_string();
    let request = QueryRequest {
        request_id: request_id.clone(),
        workspace: WorkspaceSelectorWire::Locator {
            path: absolute_workspace(&common.workspace),
        },
        correlation: common.correlation(),
        operation,
    };

    let response = match client::send(&mut connection, Request::Query(request)).await {
        Ok(Response::Query(response)) => response,
        Ok(_) => {
            eprintln!("brainprint: daemon sent an unexpected response to Query");
            return Exit::DaemonOrProtocolFailure;
        }
        Err(error) => {
            eprintln!("brainprint: {error}");
            return Exit::DaemonOrProtocolFailure;
        }
    };

    let exit = match &response.outcome {
        QueryOutcomeWire::Ok(_) => Exit::Ok,
        QueryOutcomeWire::Err(error) => {
            eprintln!("brainprint: {:?}: {}", error.code, error.message);
            exit_for_code(error.code)
        }
    };

    let output_result = if common.json {
        render::print_json(&response)
    } else if let QueryOutcomeWire::Ok(result) = &response.outcome {
        render::print_compact(result)
    } else {
        Ok(())
    };

    let Ok(()) = output_result else {
        // #24 §20/§23: output failed -- no acknowledgement.
        return Exit::DaemonOrProtocolFailure;
    };

    let Some(ack_token) = response.ack_token else {
        return exit;
    };

    let ack_request = QueryAckRequest {
        request_id,
        workspace_id: response.workspace_id,
        ack_token,
    };
    match client::send(&mut connection, Request::QueryAck(ack_request)).await {
        Ok(Response::QueryAck(ack)) => match ack.status {
            AckStatusWire::Acknowledged | AckStatusWire::AlreadyAcknowledged => exit,
            AckStatusWire::UnknownOrExpired => {
                eprintln!(
                    "brainprint: delivery acknowledgement was not accepted (unknown/expired token)"
                );
                Exit::DaemonOrProtocolFailure
            }
        },
        Ok(_) => {
            eprintln!("brainprint: daemon sent an unexpected response to QueryAck");
            Exit::DaemonOrProtocolFailure
        }
        Err(error) => {
            // #24 §23: the printed facts are not retracted, but delivery
            // is correctly unacknowledged daemon-side.
            eprintln!("brainprint: delivery acknowledgement failed: {error}");
            Exit::DaemonOrProtocolFailure
        }
    }
}

/// The selector as wire, with `--in-path` resolved to its Resource id.
async fn target_wire(
    mut target: TargetArgs,
    common: &CommonArgs,
) -> Result<ProjectionTargetWire, Exit> {
    if let Some(path) = target.in_path.take() {
        target.in_resource = Some(resource_at(common, path).await?);
    }
    target
        .into_wire()
        .map_err(|error| syntax_exit(error.into()))
}

/// #96: the id of the Resource at `path`, by one unretained find target
/// query; anything but a resolved Resource is the CLI's error, by name.
async fn resource_at(common: &CommonArgs, path: String) -> Result<ResourceId, Exit> {
    let mut connection = client::connect_and_handshake().await.map_err(|error| {
        eprintln!("brainprint: {error}");
        Exit::DaemonOrProtocolFailure
    })?;
    let request = QueryRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        workspace: WorkspaceSelectorWire::Locator {
            path: absolute_workspace(&common.workspace),
        },
        correlation: None,
        operation: QueryOperationWire::Find(FindQueryWire::Target {
            target: ProjectionTargetWire::Resource(ResourceTargetWire::Path(path.clone())),
            delivery: DeliveryWire {
                budget: DeliveryBudgetWire {
                    max_items: NonZeroUsize::new(64),
                    max_bytes: NonZeroUsize::new(64 * 1024),
                },
                continuation: None,
                retention: RetentionWire::Disabled,
            },
        }),
    };
    let response = match client::send(&mut connection, Request::Query(request)).await {
        Ok(Response::Query(response)) => response,
        Ok(_) => {
            eprintln!("brainprint: daemon sent an unexpected response to Query");
            return Err(Exit::DaemonOrProtocolFailure);
        }
        Err(error) => {
            eprintln!("brainprint: {error}");
            return Err(Exit::DaemonOrProtocolFailure);
        }
    };
    match response.outcome {
        QueryOutcomeWire::Ok(QueryResultWire::Find(FindResultWire::Target(answer))) => {
            match answer.target_resolution {
                TargetResolutionWire::Resolved(GraphEndpointWire::Resource(id)) => Ok(id),
                other => Err(syntax_exit(CliSyntaxError(format!(
                    "--in-path {path}: {other:?}"
                )))),
            }
        }
        QueryOutcomeWire::Ok(_) => {
            eprintln!("brainprint: daemon sent an unexpected result to find target");
            Err(Exit::DaemonOrProtocolFailure)
        }
        QueryOutcomeWire::Err(error) => {
            eprintln!(
                "brainprint: --in-path {path}: {:?}: {}",
                error.code, error.message
            );
            Err(exit_for_code(error.code))
        }
    }
}

fn syntax_exit(error: CliSyntaxError) -> Exit {
    eprintln!("brainprint: {}", error.0);
    Exit::CliSyntax
}

pub async fn run_find(mode: FindCommand) -> Exit {
    match mode {
        FindCommand::Target(args) => run_find_target(args).await,
        FindCommand::Files(args) => run_find_files(args).await,
        FindCommand::Text(args) => run_find_text(args).await,
    }
}

async fn run_find_target(args: FindTargetArgs) -> Exit {
    let target = match target_wire(args.target, &args.common).await {
        Ok(target) => target,
        Err(exit) => return exit,
    };
    let delivery = match args.delivery.into_wire(args.common.has_correlation()) {
        Ok(delivery) => delivery,
        Err(error) => return syntax_exit(error.into()),
    };
    run(
        &args.common,
        QueryOperationWire::Find(FindQueryWire::Target { target, delivery }),
    )
    .await
}

async fn run_find_files(args: FindFilesArgs) -> Exit {
    run(
        &args.common,
        QueryOperationWire::Find(FindQueryWire::Files {
            directory: args.directory,
            recursive: args.recursive,
            path_prefix: args.path_prefix,
            role: args.role.map(Into::into),
            language: args.language.map(Into::into),
            kind: args.kind.map(Into::into),
            limit: args.limit,
        }),
    )
    .await
}

async fn run_find_text(args: FindTextArgs) -> Exit {
    let pattern = match (args.literal, args.regex) {
        (Some(literal), None) => {
            brainprint_core::protocol::query::TextPatternWire::Literal(literal)
        }
        (None, Some(regex)) => brainprint_core::protocol::query::TextPatternWire::Regex(regex),
        _ => {
            return syntax_exit(CliSyntaxError(
                "exactly one of --literal or --regex is required".to_owned(),
            ));
        }
    };
    let (search_budget, max_file_bytes) = args.search_budget.into_wire();
    run(
        &args.common,
        QueryOperationWire::Find(FindQueryWire::Text {
            pattern,
            case_insensitive: args.case_insensitive,
            path_prefix: args.path_prefix,
            search_budget,
            max_file_bytes,
            with_preview: args.preview,
        }),
    )
    .await
}

pub async fn run_inspect(args: InspectArgs) -> Exit {
    let target = match target_wire(args.target, &args.common).await {
        Ok(target) => target,
        Err(exit) => return exit,
    };
    let delivery = match args.delivery.into_wire(args.common.has_correlation()) {
        Ok(delivery) => delivery,
        Err(error) => return syntax_exit(error.into()),
    };
    run(
        &args.common,
        QueryOperationWire::Inspect(brainprint_core::protocol::query::InspectWire {
            target,
            delivery,
        }),
    )
    .await
}

pub async fn run_relations(args: RelationsArgs) -> Exit {
    let target = match target_wire(args.target, &args.common).await {
        Ok(target) => target,
        Err(exit) => return exit,
    };
    let delivery = match args.delivery.into_wire(args.common.has_correlation()) {
        Ok(delivery) => delivery,
        Err(error) => return syntax_exit(error.into()),
    };
    run(
        &args.common,
        QueryOperationWire::Relations(brainprint_core::protocol::query::RelationsWire {
            target,
            direction: args.direction.into(),
            kinds: args.kind.into_iter().map(Into::into).collect(),
            delivery,
        }),
    )
    .await
}

pub async fn run_impact(args: ImpactArgs) -> Exit {
    let target = match target_wire(args.target, &args.common).await {
        Ok(target) => target,
        Err(exit) => return exit,
    };
    let delivery = match args.delivery.into_wire(args.common.has_correlation()) {
        Ok(delivery) => delivery,
        Err(error) => return syntax_exit(error.into()),
    };
    run(
        &args.common,
        QueryOperationWire::Impact(brainprint_core::protocol::query::ImpactWire {
            target,
            change: args.change.into(),
            delivery,
        }),
    )
    .await
}

pub async fn run_context(mode: ContextCommand) -> Exit {
    match mode {
        ContextCommand::Change(args) => run_context_change(args).await,
        ContextCommand::Resume(args) => run_context_resume(args).await,
    }
}

async fn run_context_change(args: ContextChangeArgs) -> Exit {
    let target = match target_wire(args.target, &args.common).await {
        Ok(target) => target,
        Err(exit) => return exit,
    };
    let knowledge = match args.knowledge.into_wire() {
        Ok(knowledge) => knowledge,
        Err(error) => return syntax_exit(error.into()),
    };
    let delivery = match args.delivery.into_wire(args.common.has_correlation()) {
        Ok(delivery) => delivery,
        Err(error) => return syntax_exit(error.into()),
    };
    run(
        &args.common,
        QueryOperationWire::Context(brainprint_core::protocol::query::ContextWire::Change {
            target,
            change: args.change.map(Into::into),
            work_item: args.work_item,
            scope_layers: knowledge.scope_layers,
            directives: knowledge.directives,
            knowledge_refs: knowledge.knowledge_refs,
            delivery,
        }),
    )
    .await
}

/// Exact title lookup; never silently choose one of multiple WorkItems.
fn unique_work_item_by_title(
    items: &[WorkItemWire],
    truncated: bool,
    title: &str,
) -> Result<WorkItemId, &'static str> {
    if truncated {
        return Err("work-item list was truncated; use --work-item UUID");
    }
    let mut matches = items.iter().filter(|item| item.title.as_deref() == Some(title));
    let Some(first) = matches.next() else {
        return Err("no unfinished WorkItem with this exact title; use --work-item UUID");
    };
    if matches.next().is_some() {
        return Err("multiple unfinished WorkItems have this title; use --work-item UUID");
    }
    Ok(first.uid)
}

/// Resolve a title within the same Workspace before a single resume call.
/// The intermediary WorkItem list is never printed into the agent's context.
async fn work_item_for_title(common: &CommonArgs, title: &str) -> Result<WorkItemId, Exit> {
    let mut connection = client::connect_and_handshake().await.map_err(|error| {
        eprintln!("brainprint: {error}");
        Exit::DaemonOrProtocolFailure
    })?;
    let request_id = uuid::Uuid::new_v4().to_string();
    let request = QueryRequest {
        request_id: request_id.clone(),
        workspace: WorkspaceSelectorWire::Locator {
            path: absolute_workspace(&common.workspace),
        },
        correlation: common.correlation(),
        operation: QueryOperationWire::Knowledge(KnowledgeWire::WorkItems {
            statuses: vec![
                WorkItemStatusWire::Open,
                WorkItemStatusWire::Active,
                WorkItemStatusWire::Blocked,
                WorkItemStatusWire::Paused,
            ],
            limit: NonZeroUsize::new(128).expect("128 is nonzero"),
        }),
    };
    let response = match client::send(&mut connection, Request::Query(request)).await {
        Ok(Response::Query(response)) => response,
        Ok(_) => {
            eprintln!("brainprint: unexpected response to work-item title lookup");
            return Err(Exit::DaemonOrProtocolFailure);
        }
        Err(error) => {
            eprintln!("brainprint: {error}");
            return Err(Exit::DaemonOrProtocolFailure);
        }
    };
    let selected = match &response.outcome {
        QueryOutcomeWire::Ok(QueryResultWire::Knowledge(KnowledgeResultWire::WorkItems {
            items,
            truncated,
        })) => unique_work_item_by_title(items, *truncated, title).map_err(|message| {
            eprintln!("brainprint: {message}");
            Exit::CliSyntax
        }),
        QueryOutcomeWire::Err(error) => {
            eprintln!("brainprint: {:?}: {}", error.code, error.message);
            Err(exit_for_code(error.code))
        }
        _ => {
            eprintln!("brainprint: unexpected query result for work-item title lookup");
            Err(Exit::DaemonOrProtocolFailure)
        }
    };
    if let Some(ack_token) = response.ack_token {
        let ack_request = QueryAckRequest {
            request_id,
            workspace_id: response.workspace_id,
            ack_token,
        };
        match client::send(&mut connection, Request::QueryAck(ack_request)).await {
            Ok(Response::QueryAck(ack))
                if matches!(ack.status, AckStatusWire::Acknowledged | AckStatusWire::AlreadyAcknowledged) => {}
            Ok(_) | Err(_) => {
                eprintln!("brainprint: work-item lookup acknowledgement failed");
                return Err(Exit::DaemonOrProtocolFailure);
            }
        }
    }
    selected
}

async fn run_context_resume(args: ContextResumeArgs) -> Exit {
    let work_item = match (args.work_item, args.title.as_deref()) {
        (Some(id), None) => id,
        (None, Some(title)) if !title.is_empty() => {
            match work_item_for_title(&args.common, title).await {
                Ok(id) => id,
                Err(exit) => return exit,
            }
        }
        _ => {
            return syntax_exit(CliSyntaxError(
                "provide exactly one of --work-item UUID or --title EXACT_TITLE".to_owned(),
            ));
        }
    };
    let target = if args.target.resource_id.is_none()
        && args.target.resource_path.is_none()
        && args.target.resource_basename.is_none()
        && args.target.resource_prefix.is_none()
        && args.target.symbol_id.is_none()
        && args.target.qualified.is_none()
        && args.target.symbol_name.is_none()
        && args.target.partial_symbol.is_none()
        && args.target.target_json.is_none()
    {
        None
    } else {
        match target_wire(args.target, &args.common).await {
            Ok(target) => Some(target),
            Err(exit) => return exit,
        }
    };
    let knowledge = match args.knowledge.into_wire() {
        Ok(knowledge) => knowledge,
        Err(error) => return syntax_exit(error.into()),
    };
    let delivery = match args.delivery.into_wire(args.common.has_correlation()) {
        Ok(delivery) => delivery,
        Err(error) => return syntax_exit(error.into()),
    };
    run(
        &args.common,
        QueryOperationWire::Context(brainprint_core::protocol::query::ContextWire::Resume {
            work_item,
            target,
            scope_layers: knowledge.scope_layers,
            directives: knowledge.directives,
            knowledge_refs: knowledge.knowledge_refs,
            delivery,
        }),
    )
    .await
}

pub async fn run_knowledge(mode: KnowledgeCommand) -> Exit {
    match mode {
        KnowledgeCommand::Rules(args) => run_knowledge_rules(args).await,
        KnowledgeCommand::WorkItems(args) => run_knowledge_work_items(args).await,
        KnowledgeCommand::Lineage(args) => run_knowledge_lineage(args).await,
        KnowledgeCommand::Handoffs(args) => run_knowledge_handoffs(args).await,
    }
}

async fn run_knowledge_rules(args: KnowledgeRulesArgs) -> Exit {
    let knowledge = match args.knowledge.into_wire() {
        Ok(knowledge) => knowledge,
        Err(error) => return syntax_exit(error.into()),
    };
    run(
        &args.common,
        QueryOperationWire::Knowledge(brainprint_core::protocol::query::KnowledgeWire::Rules {
            scope_layers: knowledge.scope_layers,
            directives: knowledge.directives,
            knowledge_refs: knowledge.knowledge_refs,
        }),
    )
    .await
}

async fn run_knowledge_work_items(args: KnowledgeWorkItemsArgs) -> Exit {
    run(
        &args.common,
        QueryOperationWire::Knowledge(brainprint_core::protocol::query::KnowledgeWire::WorkItems {
            statuses: args.status.into_iter().map(Into::into).collect(),
            limit: args.limit,
        }),
    )
    .await
}

async fn run_knowledge_lineage(args: KnowledgeLineageArgs) -> Exit {
    let target = match (args.project_policy, args.user_policy, args.decision) {
        (Some(id), None, None) => {
            brainprint_core::protocol::query::LineageTargetWire::ProjectPolicy(id)
        }
        (None, Some(id), None) => {
            brainprint_core::protocol::query::LineageTargetWire::UserPolicy(id)
        }
        (None, None, Some(id)) => brainprint_core::protocol::query::LineageTargetWire::Decision(id),
        _ => {
            return syntax_exit(CliSyntaxError(
                "exactly one of --project-policy, --user-policy, --decision is required".to_owned(),
            ));
        }
    };
    run(
        &args.common,
        QueryOperationWire::Knowledge(brainprint_core::protocol::query::KnowledgeWire::Lineage(
            target,
        )),
    )
    .await
}

async fn run_knowledge_handoffs(args: KnowledgeHandoffsArgs) -> Exit {
    run(
        &args.common,
        QueryOperationWire::Knowledge(brainprint_core::protocol::query::KnowledgeWire::Handoffs {
            work_item: args.work_item,
            limit: args.limit,
        }),
    )
    .await
}

pub async fn run_structure(args: StructureArgs) -> Exit {
    let grouping = match args.grouping() {
        Ok(grouping) => grouping,
        Err(_) => {
            return syntax_exit(CliSyntaxError(
                "exactly one --group-* grouping mode is required".to_owned(),
            ));
        }
    };
    let resource_scope = args.resource_scope();
    run(
        &args.common,
        QueryOperationWire::Structure(brainprint_core::protocol::query::StructureWire {
            grouping,
            resource_scope,
            relation_kinds: args.relation_kind.into_iter().map(Into::into).collect(),
            include_ungrouped: !args.exclude_ungrouped,
            include_cycles: args.include_cycles,
            member_sample_limit: args.member_sample_limit,
        }),
    )
    .await
}

#[cfg(test)]
mod title_lookup_tests {
    use super::*;

    fn item(id: u8, title: &str) -> WorkItemWire {
        WorkItemWire {
            uid: WorkItemId::from_bytes([id; 16]),
            source_kind: brainprint_core::protocol::query::WorkItemSourceKindWire::UserRequest,
            source_ref: None,
            title: Some(title.to_owned()),
            goal: "test".to_owned(),
            status: WorkItemStatusWire::Active,
            created_at: "2026-10-12".to_owned(),
            closed_at: None,
        }
    }

    #[test]
    fn exact_title_selects_one_without_guessing() {
        let one = item(1, "now_unix_ms u128");
        let other = item(2, "unrelated");
        assert_eq!(
            unique_work_item_by_title(&[one.clone(), other], false, "now_unix_ms u128"),
            Ok(one.uid)
        );
        assert!(unique_work_item_by_title(&[one.clone()], false, "not found").is_err());
        assert!(unique_work_item_by_title(&[one.clone(), one.clone()], false, "now_unix_ms u128").is_err());
        assert!(unique_work_item_by_title(&[one], true, "now_unix_ms u128").is_err());
    }
}
