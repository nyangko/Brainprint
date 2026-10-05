//! #70/#72: the daemon access the human surfaces (TUI, local Web UI)
//! share -- one request per connection, the same selector, budget,
//! candidate descriptors and compact body as the CLI. No fact is decided
//! here; every value is the daemon's response.

use std::{num::NonZeroUsize, path::PathBuf};

use brainprint_core::protocol::{
    EndpointPaths, Request, Response, StatusRequest, StatusResponse,
    maintenance::{DoctorRequest, DoctorResponse},
    query::*,
};

use crate::client::{self, CliError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub label: String,
    pub target: ProjectionTargetWire,
}

pub enum Failure {
    Connection(CliError),
    /// The daemon answered with an error about this request.
    Rejected(String),
}

/// Where a human surface's requests go.
pub struct Daemon {
    pub endpoint: EndpointPaths,
    /// Absolute Workspace locator.
    pub workspace: String,
    /// The global config, for the saved locale.
    pub config: Option<PathBuf>,
}

impl Daemon {
    pub async fn send(&self, request: Request) -> Result<Response, Failure> {
        let mut connection = client::connect(&self.endpoint)
            .await
            .map_err(Failure::Connection)?;
        client::send(&mut connection, request)
            .await
            .map_err(Failure::Connection)
    }

    pub async fn status(&self) -> Result<StatusResponse, Failure> {
        let path = Some(self.workspace.clone());
        match self.send(Request::Status(StatusRequest { path })).await? {
            Response::Status(status) => Ok(status),
            other => Err(unexpected(other)),
        }
    }

    pub async fn doctor(&self) -> Result<DoctorResponse, Failure> {
        let path = self.workspace.clone();
        match self.send(Request::Doctor(DoctorRequest { path })).await? {
            Response::Doctor(doctor) => Ok(doctor),
            other => Err(unexpected(other)),
        }
    }

    /// One query; its delivery is acknowledged on the same connection.
    pub async fn query(&self, operation: QueryOperationWire) -> Result<QueryResultWire, Failure> {
        let mut connection = client::connect(&self.endpoint)
            .await
            .map_err(Failure::Connection)?;
        let request_id = uuid::Uuid::new_v4().to_string();
        let request = Request::Query(QueryRequest {
            request_id: request_id.clone(),
            workspace: WorkspaceSelectorWire::Locator {
                path: self.workspace.clone(),
            },
            correlation: None,
            operation,
        });
        let response = match client::send(&mut connection, request)
            .await
            .map_err(Failure::Connection)?
        {
            Response::Query(response) => response,
            other => return Err(unexpected(other)),
        };
        if let Some(ack_token) = response.ack_token {
            let ack = Request::QueryAck(QueryAckRequest {
                request_id,
                workspace_id: response.workspace_id,
                ack_token,
            });
            client::send(&mut connection, ack)
                .await
                .map_err(Failure::Connection)?;
        }
        match response.outcome {
            QueryOutcomeWire::Ok(result) => Ok(result),
            QueryOutcomeWire::Err(error) => Err(Failure::Rejected(format!(
                "{:?}: {}",
                error.code, error.message
            ))),
        }
    }
}

pub fn unexpected(response: Response) -> Failure {
    match response {
        Response::Error(error) => Failure::Rejected(error.message),
        _ => Failure::Connection(CliError::UnexpectedResponse),
    }
}

/// The CLI's `standard` budget; retention off, so nothing is held for a
/// correlation a human surface does not have.
pub fn delivery(continuation: Option<DeliveryContinuationWire>) -> DeliveryWire {
    DeliveryWire {
        budget: DeliveryBudgetWire {
            max_items: NonZeroUsize::new(64),
            max_bytes: NonZeroUsize::new(64 * 1024),
        },
        continuation,
        retention: RetentionWire::Disabled,
    }
}

/// What the user typed, as a selector: a path when it looks like one,
/// otherwise a partial Symbol name. The daemon resolves either.
pub fn selector(text: &str) -> ProjectionTargetWire {
    if text.contains('/') || text.contains('.') {
        ProjectionTargetWire::Resource(ResourceTargetWire::Path(text.to_owned()))
    } else {
        ProjectionTargetWire::Symbol(SymbolTargetWire {
            name: SymbolNameWire::PartialName(text.to_owned()),
            resource: None,
            kind: None,
            language: None,
        })
    }
}

pub fn compact(result: &QueryResultWire) -> Vec<String> {
    let mut out = Vec::new();
    // Writing into a Vec cannot fail.
    let _ = crate::query::render::write_compact(&mut out, result);
    String::from_utf8_lossy(&out)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The candidate descriptors a find answer delivered, in its order.
pub fn candidates(answer: &ProjectedAnswerWire) -> Vec<Candidate> {
    let mut found: Vec<Candidate> = Vec::new();
    for item in &answer.page.evidence {
        let DeliveredItemWire::Full(evidence) = item else {
            continue;
        };
        let candidate = match evidence {
            EvidenceWire::Symbol(symbol) => Candidate {
                label: format!(
                    "{:?} {}  {}:{}",
                    symbol.symbol.kind,
                    symbol.symbol.qualified_name,
                    symbol.path_rel,
                    symbol.symbol.span.start.line + 1
                ),
                target: ProjectionTargetWire::Symbol(SymbolTargetWire {
                    name: SymbolNameWire::Id(symbol.symbol.id),
                    resource: None,
                    kind: None,
                    language: None,
                }),
            },
            EvidenceWire::Resource(resource) => Candidate {
                label: resource.path_rel.clone(),
                target: ProjectionTargetWire::Resource(ResourceTargetWire::Id(resource.id)),
            },
            _ => continue,
        };
        if !found.iter().any(|known| known.target == candidate.target) {
            found.push(candidate);
        }
    }
    found
}

/// The impact change forms the daemon accepts, in cycle order.
pub const CHANGES: [ChangeKindWire; 6] = [
    ChangeKindWire::Structural(ImpactIntentWire::PublicSignatureChange),
    ChangeKindWire::Structural(ImpactIntentWire::Rename),
    ChangeKindWire::Structural(ImpactIntentWire::ModuleMove),
    ChangeKindWire::Structural(ImpactIntentWire::BaseInterfaceChange),
    ChangeKindWire::Delete,
    ChangeKindWire::DomainContractChange,
];

/// The compact Working State summary both surfaces show: unfinished
/// WorkItems, at most five.
pub fn open_work_items() -> QueryOperationWire {
    QueryOperationWire::Knowledge(KnowledgeWire::WorkItems {
        statuses: vec![
            WorkItemStatusWire::Active,
            WorkItemStatusWire::Blocked,
            WorkItemStatusWire::Paused,
            WorkItemStatusWire::Open,
        ],
        limit: NonZeroUsize::new(5).expect("5 != 0"),
    })
}
