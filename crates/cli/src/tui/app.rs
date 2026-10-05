//! #70: TUI state, the key reducer, and the daemon calls behind it.
//!
//! A thin client: every fact on screen is a daemon response kept as the
//! wire value it arrived as. Nothing here resolves a target, decides
//! currentness or counts relations -- the view only words what came back.
//! Each request opens its own connection, so a restarted daemon is simply
//! reached by the next request, and a failed one clears every earlier
//! answer instead of leaving it on screen as if still current.

use std::{num::NonZeroUsize, path::PathBuf};

use brainprint_core::{
    present::{Locale, Msg},
    protocol::{
        EndpointPaths, Request, Response, StatusRequest, StatusResponse,
        maintenance::{
            DoctorRequest, DoctorResponse, RebuildRequest, RebuildResponse, SyncRequest,
            SyncResponse, UninitRequest, UninitResponse,
        },
        query::*,
    },
};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::client::{self, CliError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Inspect,
    Relations,
    Impact,
    Operations,
}

impl Tab {
    pub const ALL: [Self; 5] = [
        Self::Overview,
        Self::Inspect,
        Self::Relations,
        Self::Impact,
        Self::Operations,
    ];

    pub const fn msg(self) -> Msg {
        match self {
            Self::Overview => Msg::TabOverview,
            Self::Inspect => Msg::TabInspect,
            Self::Relations => Msg::TabRelations,
            Self::Impact => Msg::TabImpact,
            Self::Operations => Msg::TabOperations,
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }
}

/// The maintenance actions, with the CLI commands' own meanings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Doctor,
    Sync,
    Rebuild,
    Uninit,
}

impl Op {
    pub const ALL: [Self; 4] = [Self::Doctor, Self::Sync, Self::Rebuild, Self::Uninit];

    pub const fn msg(self) -> Msg {
        match self {
            Self::Doctor => Msg::ActionDoctor,
            Self::Sync => Msg::ActionSync,
            Self::Rebuild => Msg::ActionRebuild,
            Self::Uninit => Msg::ActionUninit,
        }
    }

    pub const fn description(self) -> Msg {
        match self {
            Self::Doctor => Msg::ActionDoctorDescription,
            Self::Sync => Msg::ActionSyncDescription,
            Self::Rebuild => Msg::ActionRebuildDescription,
            Self::Uninit => Msg::ActionUninitDescription,
        }
    }

    /// Changes the index lifecycle or detaches: asks first.
    const fn needs_confirmation(self) -> bool {
        matches!(self, Self::Rebuild | Self::Uninit)
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    NotYet,
    Connected,
    Disconnected(String),
    /// A protocol mismatch; the message names the stale side (#66).
    Incompatible(String),
}

pub struct Overview {
    pub status: StatusResponse,
    pub doctor: DoctorResponse,
    pub work: Result<KnowledgeResultWire, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub label: String,
    pub target: ProjectionTargetWire,
}

/// A bounded, possibly continued answer: the last page's facts plus the
/// compact rendering of every page fetched so far.
pub struct Paged {
    pub operation: QueryOperationWire,
    pub last: ProjectedAnswerWire,
    pub body: Vec<String>,
    pub pages: usize,
}

pub enum OpResult {
    Doctor(Box<DoctorResponse>),
    Sync(SyncResponse),
    Rebuild(RebuildResponse),
    Uninit(UninitResponse),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Refresh,
    Search(String),
    Inspect,
    More,
    Relations,
    Impact,
    Run(Op),
    SwitchLocale,
}

pub struct App {
    pub locale: Locale,
    pub locale_notice: Option<Msg>,
    pub tab: Tab,
    pub connection: Connection,
    pub help: bool,
    pub quit: bool,
    pub overview: Option<Overview>,
    pub input: Option<String>,
    pub query: String,
    pub search: Option<(TargetResolutionWire, CurrentnessWire)>,
    pub candidates: Vec<Candidate>,
    pub selected: usize,
    pub target: Option<Candidate>,
    pub inspect: Option<Paged>,
    pub relations: Option<RelationsResultWire>,
    pub relations_body: Vec<String>,
    pub change: usize,
    pub impact: Option<Paged>,
    pub op_selected: usize,
    pub confirm: Option<Op>,
    pub op_result: Option<OpResult>,
    pub error: Option<String>,
    pub scroll: u16,
}

impl App {
    pub fn new(locale: Locale) -> Self {
        Self {
            locale,
            locale_notice: None,
            tab: Tab::Overview,
            connection: Connection::NotYet,
            help: false,
            quit: false,
            overview: None,
            input: None,
            query: String::new(),
            search: None,
            candidates: Vec::new(),
            selected: 0,
            target: None,
            inspect: None,
            relations: None,
            relations_body: Vec::new(),
            change: 0,
            impact: None,
            op_selected: 0,
            confirm: None,
            op_result: None,
            error: None,
            scroll: 0,
        }
    }

    /// One key press. Returns the daemon call it asks for, if any; every
    /// state change that needs no daemon happens here.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Command> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return None;
        }
        // A pending confirmation takes every key: only `y` runs it, and
        // `q` cancels rather than quits.
        if let Some(op) = self.confirm {
            match key.code {
                KeyCode::Char('y') => {
                    self.confirm = None;
                    return Some(Command::Run(op));
                }
                KeyCode::Char('n' | 'q') | KeyCode::Esc => self.confirm = None,
                _ => {}
            }
            return None;
        }
        if self.help {
            self.help = false;
            return None;
        }
        if let Some(input) = &mut self.input {
            match key.code {
                KeyCode::Esc => self.input = None,
                KeyCode::Enter => {
                    let text = self.input.take().unwrap_or_default();
                    let text = text.trim();
                    if !text.is_empty() {
                        return Some(Command::Search(text.to_owned()));
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(character) => input.push(character),
                _ => {}
            }
            return None;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Tab => return self.switch(Tab::ALL[(self.tab.index() + 1) % Tab::ALL.len()]),
            KeyCode::BackTab => {
                let previous = (self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len();
                return self.switch(Tab::ALL[previous]);
            }
            KeyCode::Char(digit @ '1'..='5') => {
                let index = digit as usize - '1' as usize;
                return self.switch(Tab::ALL[index]);
            }
            KeyCode::Char('r') => return Some(Command::Refresh),
            KeyCode::Char('L') => return Some(Command::SwitchLocale),
            KeyCode::Up => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
            KeyCode::Char('/') if self.tab == Tab::Inspect => self.input = Some(String::new()),
            KeyCode::Char('n') => {
                let paged = match self.tab {
                    Tab::Inspect => self.inspect.as_ref(),
                    Tab::Impact => self.impact.as_ref(),
                    _ => None,
                };
                if paged.is_some_and(|paged| paged.last.more_available) {
                    return Some(Command::More);
                }
            }
            KeyCode::Char('c') if self.tab == Tab::Impact => {
                self.change = (self.change + 1) % CHANGES.len();
                self.impact = None;
                if self.target.is_some() {
                    return Some(Command::Impact);
                }
            }
            KeyCode::Enter => return self.enter(),
            _ => {}
        }
        None
    }

    fn enter(&mut self) -> Option<Command> {
        match self.tab {
            Tab::Overview => Some(Command::Refresh),
            Tab::Inspect => (!self.candidates.is_empty()).then_some(Command::Inspect),
            Tab::Relations => self.target.is_some().then_some(Command::Relations),
            Tab::Impact => self.target.is_some().then_some(Command::Impact),
            Tab::Operations => {
                let op = Op::ALL[self.op_selected];
                if op.needs_confirmation() {
                    self.confirm = Some(op);
                    None
                } else {
                    Some(Command::Run(op))
                }
            }
        }
    }

    fn switch(&mut self, tab: Tab) -> Option<Command> {
        self.tab = tab;
        self.scroll = 0;
        self.error = None;
        match tab {
            Tab::Overview if self.overview.is_none() => Some(Command::Refresh),
            Tab::Relations if self.target.is_some() && self.relations.is_none() => {
                Some(Command::Relations)
            }
            Tab::Impact if self.target.is_some() && self.impact.is_none() => Some(Command::Impact),
            _ => None,
        }
    }

    fn step(&mut self, delta: isize) {
        let moved = |value: usize, len: usize| -> usize {
            (value as isize + delta).clamp(0, len.saturating_sub(1) as isize) as usize
        };
        match self.tab {
            Tab::Inspect if !self.candidates.is_empty() && self.inspect.is_none() => {
                self.selected = moved(self.selected, self.candidates.len());
            }
            Tab::Operations => self.op_selected = moved(self.op_selected, Op::ALL.len()),
            _ => {
                self.scroll = if delta < 0 {
                    self.scroll.saturating_sub(1)
                } else {
                    self.scroll.saturating_add(1)
                };
            }
        }
    }

    /// The daemon is gone or incompatible: drop every answer it gave, so
    /// nothing stale stays on screen as if current.
    fn disconnect(&mut self, error: &CliError) {
        self.connection = match error {
            CliError::VersionMismatch { .. } => Connection::Incompatible(error.to_string()),
            _ => Connection::Disconnected(error.to_string()),
        };
        self.overview = None;
        self.search = None;
        self.candidates.clear();
        self.target = None;
        self.inspect = None;
        self.relations = None;
        self.relations_body.clear();
        self.impact = None;
        self.op_result = None;
    }

    /// The basis may have moved: answers about the old one are dropped.
    fn forget_answers(&mut self) {
        self.inspect = None;
        self.relations = None;
        self.relations_body.clear();
        self.impact = None;
    }
}

pub enum Failure {
    Connection(CliError),
    /// The daemon answered with an error about this request.
    Rejected(String),
}

/// Where the TUI's requests go.
pub struct Daemon {
    pub endpoint: EndpointPaths,
    /// Absolute Workspace locator.
    pub workspace: String,
    /// The global config, for the saved locale.
    pub config: Option<PathBuf>,
}

impl Daemon {
    async fn send(&self, request: Request) -> Result<Response, Failure> {
        let mut connection = client::connect(&self.endpoint)
            .await
            .map_err(Failure::Connection)?;
        client::send(&mut connection, request)
            .await
            .map_err(Failure::Connection)
    }

    async fn status(&self) -> Result<StatusResponse, Failure> {
        match self.send(Request::Status(StatusRequest)).await? {
            Response::Status(status) => Ok(status),
            other => Err(unexpected(other)),
        }
    }

    async fn doctor(&self) -> Result<DoctorResponse, Failure> {
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

fn unexpected(response: Response) -> Failure {
    match response {
        Response::Error(error) => Failure::Rejected(error.message),
        _ => Failure::Connection(CliError::UnexpectedResponse),
    }
}

/// The CLI's `standard` budget; retention off, so nothing is held for a
/// correlation the TUI does not have.
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

fn compact(result: &QueryResultWire) -> Vec<String> {
    let mut out = Vec::new();
    // Writing into a Vec cannot fail.
    let _ = crate::query::render::write_compact(&mut out, result);
    String::from_utf8_lossy(&out)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The candidate descriptors a find answer delivered, in its order.
fn candidates(answer: &ProjectedAnswerWire) -> Vec<Candidate> {
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

/// Run `command` against the daemon and fold the outcome into `app`.
pub async fn perform(app: &mut App, daemon: &Daemon, command: Command) {
    app.error = None;
    if command == Command::SwitchLocale {
        app.locale = app.locale.next();
        app.locale_notice = Some(
            match daemon
                .config
                .as_deref()
                .map(|path| super::config::save(path, app.locale.tag()))
            {
                Some(Ok(())) => Msg::LocaleSaved,
                _ => Msg::LocaleNotSaved,
            },
        );
        return;
    }
    match run(app, daemon, command).await {
        Ok(()) => app.connection = Connection::Connected,
        Err(Failure::Connection(error)) => app.disconnect(&error),
        Err(Failure::Rejected(message)) => {
            app.connection = Connection::Connected;
            app.error = Some(message);
        }
    }
}

async fn refresh_overview(app: &mut App, daemon: &Daemon) -> Result<(), Failure> {
    let status = daemon.status().await?;
    let doctor = daemon.doctor().await?;
    let work = match daemon
        .query(QueryOperationWire::Knowledge(KnowledgeWire::WorkItems {
            statuses: vec![
                WorkItemStatusWire::Active,
                WorkItemStatusWire::Blocked,
                WorkItemStatusWire::Paused,
                WorkItemStatusWire::Open,
            ],
            limit: NonZeroUsize::new(5).expect("5 != 0"),
        }))
        .await
    {
        Ok(result) => match result {
            QueryResultWire::Knowledge(knowledge) => Ok(knowledge),
            _ => Err("unexpected Working State answer".to_owned()),
        },
        Err(Failure::Rejected(message)) => Err(message),
        Err(failure @ Failure::Connection(_)) => return Err(failure),
    };
    app.overview = Some(Overview {
        status,
        doctor,
        work,
    });
    Ok(())
}

async fn fetch_paged(
    daemon: &Daemon,
    operation: QueryOperationWire,
) -> Result<(ProjectedAnswerWire, Vec<String>), Failure> {
    let result = daemon.query(operation).await?;
    let body = compact(&result);
    match result {
        QueryResultWire::Inspect(answer) | QueryResultWire::Impact(answer) => Ok((answer, body)),
        _ => Err(Failure::Connection(CliError::UnexpectedResponse)),
    }
}

async fn run(app: &mut App, daemon: &Daemon, command: Command) -> Result<(), Failure> {
    match command {
        Command::SwitchLocale => Ok(()),
        Command::Refresh => {
            refresh_overview(app, daemon).await?;
            if app.target.is_some() {
                let refetch = match app.tab {
                    Tab::Inspect if app.inspect.is_some() => Some(Command::Inspect),
                    Tab::Relations => Some(Command::Relations),
                    Tab::Impact => Some(Command::Impact),
                    _ => None,
                };
                if let Some(command) = refetch {
                    return Box::pin(run(app, daemon, command)).await;
                }
            }
            Ok(())
        }
        Command::Search(text) => {
            app.query.clone_from(&text);
            app.forget_answers();
            app.target = None;
            app.candidates.clear();
            app.selected = 0;
            let result = daemon
                .query(QueryOperationWire::Find(FindQueryWire::Target {
                    target: selector(&text),
                    delivery: delivery(None),
                }))
                .await?;
            let QueryResultWire::Find(FindResultWire::Target(answer)) = result else {
                return Err(Failure::Connection(CliError::UnexpectedResponse));
            };
            app.candidates = candidates(&answer);
            app.search = Some((answer.target_resolution, answer.currentness));
            Ok(())
        }
        Command::Inspect => {
            let Some(candidate) = app
                .candidates
                .get(app.selected)
                .cloned()
                .or(app.target.clone())
            else {
                return Ok(());
            };
            app.forget_answers();
            let operation = QueryOperationWire::Inspect(InspectWire {
                target: candidate.target.clone(),
                delivery: delivery(None),
            });
            let (last, body) = fetch_paged(daemon, operation.clone()).await?;
            app.target = Some(candidate);
            app.inspect = Some(Paged {
                operation,
                last,
                body,
                pages: 1,
            });
            app.scroll = 0;
            Ok(())
        }
        Command::More => {
            let paged = match app.tab {
                Tab::Impact => app.impact.as_mut(),
                _ => app.inspect.as_mut(),
            };
            let Some(paged) = paged else {
                return Ok(());
            };
            let Some(continuation) = paged.last.continuation.clone() else {
                return Ok(());
            };
            let mut operation = paged.operation.clone();
            match &mut operation {
                QueryOperationWire::Inspect(inspect) => {
                    inspect.delivery.continuation = Some(continuation);
                }
                QueryOperationWire::Impact(impact) => {
                    impact.delivery.continuation = Some(continuation);
                }
                _ => return Ok(()),
            }
            let (last, body) = fetch_paged(daemon, operation).await?;
            paged.body.push(String::new());
            paged.body.extend(body);
            paged.last = last;
            paged.pages += 1;
            Ok(())
        }
        Command::Relations => {
            let Some(target) = app.target.clone() else {
                return Ok(());
            };
            let result = daemon
                .query(QueryOperationWire::Relations(RelationsWire {
                    target: target.target,
                    direction: RelationDirectionWire::Both,
                    kinds: Vec::new(),
                }))
                .await?;
            app.relations_body = compact(&result);
            let QueryResultWire::Relations(relations) = result else {
                return Err(Failure::Connection(CliError::UnexpectedResponse));
            };
            app.relations = Some(relations);
            Ok(())
        }
        Command::Impact => {
            let Some(target) = app.target.clone() else {
                return Ok(());
            };
            let operation = QueryOperationWire::Impact(ImpactWire {
                target: target.target,
                change: CHANGES[app.change],
                delivery: delivery(None),
            });
            let (last, body) = fetch_paged(daemon, operation.clone()).await?;
            app.impact = Some(Paged {
                operation,
                last,
                body,
                pages: 1,
            });
            Ok(())
        }
        Command::Run(op) => {
            let path = daemon.workspace.clone();
            let result = match op {
                Op::Doctor => OpResult::Doctor(Box::new(daemon.doctor().await?)),
                Op::Sync => match daemon.send(Request::Sync(SyncRequest { path })).await? {
                    Response::Sync(sync) => OpResult::Sync(sync),
                    other => return Err(unexpected(other)),
                },
                Op::Rebuild => match daemon
                    .send(Request::Rebuild(RebuildRequest { path }))
                    .await?
                {
                    Response::Rebuild(rebuild) => OpResult::Rebuild(rebuild),
                    other => return Err(unexpected(other)),
                },
                Op::Uninit => match daemon.send(Request::Uninit(UninitRequest { path })).await? {
                    Response::Uninit(uninit) => OpResult::Uninit(uninit),
                    other => return Err(unexpected(other)),
                },
            };
            if op != Op::Doctor {
                app.forget_answers();
            }
            app.op_result = Some(result);
            refresh_overview(app, daemon).await
        }
    }
}
