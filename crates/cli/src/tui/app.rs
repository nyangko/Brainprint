//! #70: TUI state, the key reducer, and the daemon calls behind it.
//!
//! A thin client: every fact on screen is a daemon response kept as the
//! wire value it arrived as. Nothing here resolves a target, decides
//! currentness or counts relations -- the view only words what came back.
//! Each request opens its own connection, so a restarted daemon is simply
//! reached by the next request, and a failed one clears every earlier
//! answer instead of leaving it on screen as if still current.

use brainprint_core::{
    present::{Locale, Msg},
    protocol::{
        Request, Response, StatusResponse,
        maintenance::{
            DoctorResponse, RebuildRequest, RebuildResponse, SyncRequest, SyncResponse,
            UninitRequest, UninitResponse,
        },
        query::*,
    },
};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub use crate::surface::{CHANGES, Candidate, Daemon};
use crate::{
    client::CliError,
    surface::{Failure, candidates, compact, delivery, selector, unexpected},
};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    NotYet,
    Connected,
    Disconnected(String),
    /// A protocol mismatch; the message names the stale side (#66).
    Incompatible(String),
}

/// The Overview: `status <path>` (compact, read-only) and the Working
/// State summary. The full diagnosis is the Doctor operation.
pub struct Overview {
    pub status: StatusResponse,
    pub work: Result<KnowledgeResultWire, String>,
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
    let work = match daemon.query(crate::surface::open_work_items()).await {
        Ok(result) => match result {
            QueryResultWire::Knowledge(knowledge) => Ok(knowledge),
            _ => Err("unexpected Working State answer".to_owned()),
        },
        Err(Failure::Rejected(message)) => Err(message),
        Err(failure @ Failure::Connection(_)) => return Err(failure),
    };
    app.overview = Some(Overview { status, work });
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
