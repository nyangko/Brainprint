//! #72: the Web UI's JSON. Each endpoint asks the daemon exactly what the
//! TUI asks, and returns the answer with the `present` message key for
//! every fact the page words. Every reply says how the daemon connection
//! went: `connected` carries `data`; `disconnected` / `incompatible` carry
//! none, so the page can never keep an old answer as current.

use brainprint_core::{
    PROTOCOL_VERSION,
    present::{self, Locale, Msg, text},
    protocol::{
        maintenance::{BackendStateWire, RuntimeCheckWire, StoredBasisWire, WorkspaceStatusWire},
        query::*,
    },
};
use serde_json::{Value, json};

use crate::{
    client::CliError,
    query::{decode_continuation, encode_continuation},
    surface::{CHANGES, Daemon, Failure, candidates, compact, delivery, open_work_items, selector},
};

/// Every message in `locale`, by key -- the page has no words of its own.
pub fn catalogue(locale: Locale) -> Value {
    let messages: serde_json::Map<String, Value> = Msg::ALL
        .iter()
        .map(|msg| (msg.key().to_owned(), Value::from(text(locale, *msg))))
        .collect();
    json!({
        "locale": locale.tag(),
        "locales": Locale::ALL.iter().map(|locale| locale.tag()).collect::<Vec<_>>(),
        "messages": messages,
        // The impact change forms, in the order `change=<index>` takes.
        "changes": CHANGES.map(|change| present::change_kind(change).key()),
    })
}

fn envelope(result: Result<Value, Failure>) -> Value {
    match result {
        Ok(data) => json!({ "connection": "connected", "data": data }),
        Err(Failure::Rejected(message)) => json!({ "connection": "connected", "error": message }),
        Err(Failure::Connection(error @ CliError::VersionMismatch { .. })) => {
            json!({ "connection": "incompatible", "detail": error.to_string() })
        }
        Err(Failure::Connection(error)) => {
            json!({ "connection": "disconnected", "detail": error.to_string() })
        }
    }
}

fn unexpected() -> Failure {
    Failure::Connection(CliError::UnexpectedResponse)
}

/// No Workspace-wide readiness exists for these: each answer states its
/// own currentness and coverage, so the row says exactly that.
fn capabilities() -> Vec<Value> {
    [
        Msg::CapabilityFiles,
        Msg::CapabilityStructure,
        Msg::CapabilityRelations,
        Msg::CapabilityImpact,
    ]
    .iter()
    .map(|msg| json!({ "name": msg.key(), "state": Msg::CapabilityPerQuery.key() }))
    .collect()
}

/// `status <path>` with the keys of its states.
pub async fn status(daemon: &Daemon) -> Value {
    envelope(
        async {
            let status = daemon.status().await?;
            let keys = match &status.workspace {
                Some(WorkspaceStatusWire::Initialized(report)) => {
                    let watcher = match &report.runtime {
                        RuntimeCheckWire::Active { watcher } => {
                            Some(present::watcher(watcher).key())
                        }
                        _ => None,
                    };
                    json!({
                        "workspace": Msg::WorkspaceCurrent.key(),
                        "currentness": present::index(&report.index).key(),
                        "runtime": present::runtime(&report.runtime).key(),
                        "watcher": watcher,
                        "basis": match &report.basis {
                            StoredBasisWire::Stable(_) => None,
                            StoredBasisWire::NeverPublished => Some(Msg::BasisNeverPublished.key()),
                            StoredBasisWire::Unreadable { .. } => Some(Msg::BasisUnreadable.key()),
                        },
                        "backends": report.semantic.iter().map(|backend| json!({
                            "family": backend.family,
                            "state": present::backend(&backend.state).key(),
                            "reason": match &backend.state {
                                BackendStateWire::Unavailable { reason } => Some(reason),
                                BackendStateWire::Registered => None,
                            },
                        })).collect::<Vec<_>>(),
                        "capabilities": capabilities(),
                    })
                }
                Some(WorkspaceStatusWire::NotInitialized { .. }) => {
                    json!({ "workspace": Msg::WorkspaceNotInitialized.key() })
                }
                Some(WorkspaceStatusWire::Ambiguous { .. }) => {
                    json!({ "workspace": Msg::WorkspaceAmbiguous.key() })
                }
                None => json!({}),
            };
            Ok(json!({
                "status": status,
                "client_protocol_version": PROTOCOL_VERSION,
                "keys": keys,
            }))
        }
        .await,
    )
}

/// The compact Working State summary.
pub async fn work(daemon: &Daemon) -> Value {
    envelope(
        async {
            let QueryResultWire::Knowledge(KnowledgeResultWire::WorkItems { items, truncated }) =
                daemon.query(open_work_items()).await?
            else {
                return Err(unexpected());
            };
            Ok(json!({
                "items": items.iter().map(|item| json!({
                    "item": item,
                    "status": present::work_status(item.status).key(),
                })).collect::<Vec<_>>(),
                "truncated": truncated,
            }))
        }
        .await,
    )
}

/// The rules and decisions that apply, as the daemon resolved them.
pub async fn rules(daemon: &Daemon) -> Value {
    envelope(
        async {
            let QueryResultWire::Knowledge(KnowledgeResultWire::Rules { evidence, gaps }) = daemon
                .query(QueryOperationWire::Knowledge(KnowledgeWire::Rules {
                    scope_layers: Vec::new(),
                    directives: Vec::new(),
                    knowledge_refs: ProjectionKnowledgeRefsWire::default(),
                }))
                .await?
            else {
                return Err(unexpected());
            };
            let policies: Vec<_> = evidence
                .iter()
                .filter_map(|item| match item {
                    EvidenceWire::Policy(resolved) => Some(&resolved.item),
                    _ => None,
                })
                .collect();
            let decisions: Vec<_> = evidence
                .iter()
                .filter_map(|item| match item {
                    EvidenceWire::Decision(resolved) => Some(&resolved.item),
                    _ => None,
                })
                .collect();
            Ok(json!({ "policies": policies, "decisions": decisions, "gaps": gaps.len() }))
        }
        .await,
    )
}

/// A browser-held target: only the id-based selectors this bridge itself
/// handed out (a candidate's token), never a path or a name.
pub fn target(token: &str) -> Option<ProjectionTargetWire> {
    match serde_json::from_str(token).ok()? {
        target @ ProjectionTargetWire::Resource(ResourceTargetWire::Id(_)) => Some(target),
        ProjectionTargetWire::Symbol(SymbolTargetWire {
            name: name @ SymbolNameWire::Id(_),
            ..
        }) => Some(ProjectionTargetWire::Symbol(SymbolTargetWire {
            name,
            resource: None,
            kind: None,
            language: None,
        })),
        _ => None,
    }
}

pub fn continuation(token: &str) -> Option<DeliveryContinuationWire> {
    decode_continuation(token).ok()
}

/// Search: the daemon's candidates, each with its id-based token.
pub async fn find(daemon: &Daemon, text: &str) -> Value {
    envelope(
        async {
            let QueryResultWire::Find(FindResultWire::Target(answer)) = daemon
                .query(QueryOperationWire::Find(FindQueryWire::Target {
                    target: selector(text),
                    delivery: delivery(None),
                }))
                .await?
            else {
                return Err(unexpected());
            };
            Ok(json!({
                "resolution": present::resolution(&answer.target_resolution).key(),
                "currentness": present::currentness(&answer.currentness).key(),
                "candidates": candidates(&answer).iter().map(|candidate| json!({
                    "label": candidate.label,
                    "token": serde_json::to_string(&candidate.target).expect("serializes"),
                })).collect::<Vec<_>>(),
            }))
        }
        .await,
    )
}

/// One bounded page, with what the daemon said about the rest of it.
fn page(result: &QueryResultWire, answer: &ProjectedAnswerWire) -> Value {
    json!({
        "currentness": present::currentness(&answer.currentness).key(),
        "resolution": present::resolution(&answer.target_resolution).key(),
        "more_available": answer.more_available,
        "omitted_items": answer.economy.omitted_items,
        "continuation": answer.continuation.as_ref().map(encode_continuation),
        "body": compact(result),
    })
}

pub async fn inspect(
    daemon: &Daemon,
    target: ProjectionTargetWire,
    continuation: Option<DeliveryContinuationWire>,
) -> Value {
    envelope(
        async {
            let result = daemon
                .query(QueryOperationWire::Inspect(InspectWire {
                    target,
                    delivery: delivery(continuation),
                }))
                .await?;
            let QueryResultWire::Inspect(answer) = &result else {
                return Err(unexpected());
            };
            Ok(page(&result, answer))
        }
        .await,
    )
}

pub async fn impact(
    daemon: &Daemon,
    target: ProjectionTargetWire,
    change: usize,
    continuation: Option<DeliveryContinuationWire>,
) -> Value {
    let change = CHANGES[change.min(CHANGES.len() - 1)];
    envelope(
        async {
            let result = daemon
                .query(QueryOperationWire::Impact(ImpactWire {
                    target,
                    change,
                    delivery: delivery(continuation),
                }))
                .await?;
            let QueryResultWire::Impact(answer) = &result else {
                return Err(unexpected());
            };
            let mut page = page(&result, answer);
            page["change"] = Value::from(present::change_kind(change).key());
            Ok(page)
        }
        .await,
    )
}

/// Both directions, each summarized the way every surface words it.
pub async fn relations(daemon: &Daemon, target: ProjectionTargetWire) -> Value {
    envelope(
        async {
            let result = daemon
                .query(QueryOperationWire::Relations(RelationsWire {
                    target,
                    direction: RelationDirectionWire::Both,
                    kinds: Vec::new(),
                }))
                .await?;
            let QueryResultWire::Relations(relations) = &result else {
                return Err(unexpected());
            };
            let answers: Vec<_> = relations
                .answers
                .iter()
                .map(|answer| {
                    let summary = present::relation_summary(answer);
                    json!({
                        "direction": summary.direction.key(),
                        "confirmed": summary.confirmed,
                        "kinds": summary.kinds.iter().map(|(kind, count)| json!({
                            "kind": kind,
                            "count": count,
                        })).collect::<Vec<_>>(),
                        "coverage": summary.coverage.key(),
                        "none": summary.none.map(Msg::key),
                        "gaps": summary.gaps,
                    })
                })
                .collect();
            Ok(json!({
                "currentness": present::currentness(&relations.currentness).key(),
                "resolution": present::resolution(&relations.target).key(),
                "answers": answers,
                "body": compact(&result),
            }))
        }
        .await,
    )
}
