//! #70: drawing. A pure function of [`App`]: labels come from the shared
//! catalogue (`brainprint_core::present`), facts are the daemon's values
//! as delivered, evidence bodies are the CLI's own compact rendering.
//! Lines are truncated at the panel edge, never wrapped into each other.

use brainprint_core::{
    PROTOCOL_VERSION,
    present::{self, Msg, text},
    protocol::{
        maintenance::{
            BackendCheckWire, BackendStateWire, CheckWire, DatabaseCheckWire, DoctorResponse,
            DoctorWorkspaceWire, IndexCheckWire, RuntimeCheckWire, StoredBasisWire,
            WatcherCheckWire, WorkspaceStatusWire,
        },
        query::{KnowledgeResultWire, RelationAnswerWire},
        work::PostCommandRefreshWire,
    },
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
};

use super::app::{App, CHANGES, Connection, Op, OpResult, Paged, Tab};

pub fn render(frame: &mut Frame, app: &App) {
    let [top, main, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    frame.render_widget(Paragraph::new(tabs_line(app)), top);
    let title = format!(" {} ", t(app, app.tab.msg()));
    frame.render_widget(
        Paragraph::new(body(app))
            .block(Block::bordered().title(title))
            .scroll((app.scroll, 0)),
        main,
    );
    frame.render_widget(Paragraph::new(t(app, Msg::HintKeys)), bottom);

    if app.help {
        overlay(frame, help(app));
    }
    if let Some(op) = app.confirm {
        overlay(frame, confirm(app, op));
    }
}

fn t(app: &App, msg: Msg) -> &'static str {
    text(app.locale, msg)
}

fn field(app: &App, label: Msg, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{}: ", t(app, label)),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(value.into()),
    ])
}

/// A state word plus the daemon's own detail, untranslated.
fn with_detail(app: &App, msg: Msg, detail: Option<&str>) -> String {
    match detail {
        Some(detail) => format!("{} -- {detail}", t(app, msg)),
        None => t(app, msg).to_owned(),
    }
}

fn tabs_line(app: &App) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, tab) in Tab::ALL.into_iter().enumerate() {
        let label = format!(" {} {} ", index + 1, t(app, tab.msg()));
        let style = if tab == app.tab {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        spans.push(Span::styled(label, style));
    }
    let state = match app.connection {
        Connection::NotYet => Msg::ConnectionNotYet,
        Connection::Connected => Msg::ConnectionConnected,
        Connection::Disconnected(_) => Msg::ConnectionDisconnected,
        Connection::Incompatible(_) => Msg::ConnectionIncompatible,
    };
    spans.push(Span::raw(format!(
        " | {} | {}",
        t(app, state),
        app.locale.tag()
    )));
    Line::from(spans)
}

fn body(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Connection::Disconnected(detail) | Connection::Incompatible(detail) = &app.connection {
        lines.push(Line::from(detail.clone()));
        lines.push(Line::from(t(app, Msg::ConnectionCleared)));
        lines.push(Line::from(t(app, Msg::ConnectionReconnectHint)));
        lines.push(Line::default());
    }
    if let Some(notice) = app.locale_notice {
        lines.push(field(app, Msg::LabelLocale, t(app, notice)));
    }
    if let Some(error) = &app.error {
        lines.push(field(app, Msg::ResultError, error.clone()));
        lines.push(Line::default());
    }
    match app.tab {
        Tab::Overview => overview(app, &mut lines),
        Tab::Inspect => inspect(app, &mut lines),
        Tab::Relations => relations(app, &mut lines),
        Tab::Impact => impact(app, &mut lines),
        Tab::Operations => operations(app, &mut lines),
    }
    lines
}

fn overview(app: &App, lines: &mut Vec<Line<'static>>) {
    let Some(overview) = &app.overview else {
        return;
    };
    let status = &overview.status;
    lines.push(field(
        app,
        Msg::LabelDaemon,
        format!("brainprintd {} (pid {})", status.daemon_version, status.pid),
    ));
    lines.push(field(
        app,
        Msg::LabelProtocol,
        format!(
            "{} / {} {PROTOCOL_VERSION}",
            status.protocol_version,
            t(app, Msg::LabelClientProtocol)
        ),
    ));
    lines.push(field(
        app,
        Msg::LabelUptime,
        status.uptime_seconds.to_string(),
    ));
    match &status.workspace {
        Some(workspace) => workspace_status(app, workspace, lines),
        None => lines.push(field(
            app,
            Msg::LabelWorkspace,
            t(app, Msg::MetricNotReported),
        )),
    }

    lines.push(Line::default());
    lines.push(field(app, Msg::LabelWorkingState, ""));
    match &overview.work {
        Ok(KnowledgeResultWire::WorkItems { items, truncated }) => {
            if items.is_empty() {
                lines.push(Line::from(format!("  {}", t(app, Msg::WorkNone))));
            }
            for item in items {
                let name = item.title.as_deref().unwrap_or(&item.goal);
                let source = item
                    .source_ref
                    .as_deref()
                    .map(|source| format!(" ({source})"))
                    .unwrap_or_default();
                lines.push(Line::from(format!(
                    "  [{}] {name}{source}",
                    t(app, present::work_status(item.status))
                )));
            }
            if *truncated {
                lines.push(Line::from(format!("  {}", t(app, Msg::WorkTruncated))));
            }
        }
        Ok(_) => {}
        Err(message) => lines.push(Line::from(format!("  {message}"))),
    }
}

/// A Workspace that did not resolve: the typed reason, as delivered.
fn unresolved(app: &App, path: &str, msg: Msg, detail: &str, lines: &mut Vec<Line<'static>>) {
    lines.push(field(
        app,
        Msg::LabelWorkspace,
        with_detail(app, msg, Some(detail)),
    ));
    lines.push(field(app, Msg::LabelRoot, path.to_owned()));
}

/// `status <path>`: identity, the stored basis, currentness only as an
/// active runtime holds it, runtime/watcher, capabilities, backends.
fn workspace_status(app: &App, status: &WorkspaceStatusWire, lines: &mut Vec<Line<'static>>) {
    let report = match status {
        WorkspaceStatusWire::NotInitialized { path, reason } => {
            return unresolved(app, path, Msg::WorkspaceNotInitialized, reason, lines);
        }
        WorkspaceStatusWire::Ambiguous { path, workspaces } => {
            let candidates = workspaces.join(", ");
            return unresolved(app, path, Msg::WorkspaceAmbiguous, &candidates, lines);
        }
        WorkspaceStatusWire::Initialized(report) => report,
    };
    lines.push(field(app, Msg::LabelProject, report.project_id.clone()));
    lines.push(field(app, Msg::LabelWorkspace, report.workspace_id.clone()));
    lines.push(field(app, Msg::LabelRoot, report.workspace_root.clone()));
    match &report.basis {
        StoredBasisWire::Stable(basis) => {
            lines.push(field(
                app,
                Msg::LabelRevision,
                basis.workspace_revision.clone(),
            ));
            lines.push(field(
                app,
                Msg::LabelGeneration,
                format!(
                    "{} ({} {})",
                    basis.generation_no,
                    t(app, Msg::LabelBasisRevision),
                    basis.generation_basis_revision
                ),
            ));
            lines.push(field(
                app,
                Msg::LabelIncarnation,
                basis.index_incarnation.to_string(),
            ));
        }
        StoredBasisWire::NeverPublished => lines.push(field(
            app,
            Msg::LabelGeneration,
            t(app, Msg::BasisNeverPublished),
        )),
        StoredBasisWire::Unreadable { detail } => lines.push(field(
            app,
            Msg::LabelGeneration,
            with_detail(app, Msg::BasisUnreadable, Some(detail)),
        )),
    }
    lines.push(field(
        app,
        Msg::LabelCurrentness,
        index_text(app, &report.index),
    ));
    runtime_lines(app, &report.runtime, lines);
    // A Workspace-wide state for these would be a guess: each answer
    // carries its own currentness and coverage.
    lines.push(field(app, Msg::LabelCapabilities, ""));
    for capability in [
        Msg::CapabilityFiles,
        Msg::CapabilityStructure,
        Msg::CapabilityRelations,
        Msg::CapabilityImpact,
    ] {
        lines.push(Line::from(format!(
            "  {:<11} {}",
            t(app, capability),
            t(app, Msg::CapabilityPerQuery)
        )));
    }
    backends(app, &report.semantic, lines);
}

fn index_text(app: &App, index: &IndexCheckWire) -> String {
    let detail = match index {
        IndexCheckWire::Current => None,
        IndexCheckWire::NotCurrent { detail } => Some(detail.as_str()),
        IndexCheckWire::NotMeasured { reason } => Some(reason.as_str()),
    };
    with_detail(app, present::index(index), detail)
}

fn runtime_lines(app: &App, runtime: &RuntimeCheckWire, lines: &mut Vec<Line<'static>>) {
    let (detail, watcher) = match runtime {
        RuntimeCheckWire::Active { watcher } => (None, Some(watcher)),
        RuntimeCheckWire::Unavailable { detail } => (Some(detail.as_str()), None),
        RuntimeCheckWire::Inactive => (None, None),
    };
    lines.push(field(
        app,
        Msg::LabelRuntime,
        with_detail(app, present::runtime(runtime), detail),
    ));
    if let Some(watcher) = watcher {
        lines.push(field(app, Msg::LabelWatcher, watcher_text(app, watcher)));
    }
}

fn backends(app: &App, semantic: &[BackendCheckWire], lines: &mut Vec<Line<'static>>) {
    lines.push(field(app, Msg::LabelSemantic, ""));
    for backend in semantic {
        let detail = match &backend.state {
            BackendStateWire::Unavailable { reason } => Some(reason.as_str()),
            BackendStateWire::Registered => None,
        };
        lines.push(Line::from(format!(
            "  {:<11} {}",
            backend.family,
            with_detail(app, present::backend(&backend.state), detail)
        )));
    }
}

/// The Doctor operation: the full diagnosis.
fn doctor(app: &App, doctor: &DoctorResponse, lines: &mut Vec<Line<'static>>) {
    let workspace = match &doctor.workspace {
        DoctorWorkspaceWire::NotInitialized { path, reason } => {
            return unresolved(app, path, Msg::WorkspaceNotInitialized, reason, lines);
        }
        DoctorWorkspaceWire::Ambiguous { path, workspaces } => {
            let candidates = workspaces.join(", ");
            return unresolved(app, path, Msg::WorkspaceAmbiguous, &candidates, lines);
        }
        DoctorWorkspaceWire::Initialized(workspace) => workspace,
    };
    lines.push(field(app, Msg::LabelProject, workspace.project_id.clone()));
    lines.push(field(
        app,
        Msg::LabelWorkspace,
        workspace.workspace_id.clone(),
    ));
    lines.push(field(app, Msg::LabelRoot, workspace.workspace_root.clone()));
    lines.push(field(
        app,
        Msg::LabelProjectHome,
        workspace.is_project_home.to_string(),
    ));
    lines.push(field(
        app,
        Msg::LabelBinding,
        check(app, &workspace.binding),
    ));
    runtime_lines(app, &workspace.runtime, lines);
    lines.push(field(
        app,
        Msg::LabelIndex,
        index_text(app, &workspace.index),
    ));
    lines.push(field(app, Msg::LabelDatabases, ""));
    database(app, &doctor.global_db, lines);
    for db in &workspace.databases {
        database(app, db, lines);
    }
    backends(app, &workspace.semantic, lines);
}

fn check(app: &App, value: &CheckWire) -> String {
    let detail = match value {
        CheckWire::Ok => None,
        CheckWire::Failed { detail } => Some(detail.as_str()),
        CheckWire::NotMeasured { reason } => Some(reason.as_str()),
    };
    with_detail(app, present::check(value), detail)
}

fn watcher_text(app: &App, watcher: &WatcherCheckWire) -> String {
    let detail = match watcher {
        WatcherCheckWire::Unavailable { reason } => Some(reason.as_str()),
        WatcherCheckWire::Attached | WatcherCheckWire::NotStarted => None,
    };
    with_detail(app, present::watcher(watcher), detail)
}

fn database(app: &App, db: &DatabaseCheckWire, lines: &mut Vec<Line<'static>>) {
    use brainprint_core::protocol::maintenance::{DatabaseStateWire, SchemaCheckWire};
    let version = match &db.state {
        DatabaseStateWire::Opened { schema, .. } => match schema {
            SchemaCheckWire::Current { version } => format!(" ({version})"),
            SchemaCheckWire::MigrationPending { version, latest }
            | SchemaCheckWire::Newer { version, latest } => format!(" ({version} / {latest})"),
            SchemaCheckWire::LedgerMismatch { detail } => format!(" -- {detail}"),
        },
        DatabaseStateWire::Unreadable { detail } => format!(" -- {detail}"),
        DatabaseStateWire::Missing => String::new(),
    };
    lines.push(Line::from(format!(
        "  {:<11} {}{version}",
        db.kind,
        t(app, present::database(&db.state))
    )));
}

fn target_line(app: &App, lines: &mut Vec<Line<'static>>) -> bool {
    match &app.target {
        Some(target) => {
            lines.push(field(app, Msg::LabelTarget, target.label.clone()));
            true
        }
        None => {
            lines.push(Line::from(t(app, Msg::HintSelectTarget)));
            false
        }
    }
}

fn paged(app: &App, paged: &Paged, lines: &mut Vec<Line<'static>>) {
    lines.push(field(
        app,
        Msg::LabelCurrentness,
        t(app, present::currentness(&paged.last.currentness)),
    ));
    lines.push(field(
        app,
        Msg::LabelResolution,
        t(app, present::resolution(&paged.last.target_resolution)),
    ));
    lines.push(field(app, Msg::LabelPage, paged.pages.to_string()));
    lines.push(Line::from(t(
        app,
        if paged.last.more_available {
            Msg::DeliveryMoreAvailable
        } else {
            Msg::DeliveryComplete
        },
    )));
    lines.push(Line::default());
    lines.extend(paged.body.iter().cloned().map(Line::from));
}

fn inspect(app: &App, lines: &mut Vec<Line<'static>>) {
    let shown = app
        .input
        .as_ref()
        .map_or(app.query.clone(), |input| format!("{input}_"));
    lines.push(field(app, Msg::LabelSearch, shown));
    if let Some((resolution, currentness)) = &app.search {
        lines.push(field(
            app,
            Msg::LabelCandidates,
            format!(
                "{} · {}",
                t(app, present::resolution(resolution)),
                t(app, present::currentness(currentness))
            ),
        ));
    }
    if let Some(paged_answer) = &app.inspect {
        target_line(app, lines);
        paged(app, paged_answer, lines);
        return;
    }
    if app.candidates.is_empty() {
        lines.push(Line::from(t(app, Msg::HintSearch)));
    }
    for (index, candidate) in app.candidates.iter().enumerate() {
        let marker = if index == app.selected { "> " } else { "  " };
        lines.push(Line::from(format!("{marker}{}", candidate.label)));
    }
}

/// One direction's answer. Zero confirmed relations is a count only
/// under complete coverage; otherwise the coverage state is the answer.
fn relation_answer(app: &App, answer: &RelationAnswerWire) -> Line<'static> {
    let summary = present::relation_summary(answer);
    let value = match summary.none {
        Some(none) => t(app, none).to_owned(),
        None => {
            let kinds: Vec<String> = summary
                .kinds
                .iter()
                .map(|(kind, seen)| format!("{kind:?} {seen}"))
                .collect();
            format!(
                "{} {} ({}) · {}: {}",
                t(app, Msg::LabelConfirmed),
                summary.confirmed,
                kinds.join(", "),
                t(app, Msg::LabelCoverage),
                t(app, summary.coverage)
            )
        }
    };
    let gaps = if summary.gaps > 0 {
        format!(" · {}: {}", t(app, Msg::LabelGaps), summary.gaps)
    } else {
        String::new()
    };
    field(app, summary.direction, format!("{value}{gaps}"))
}

fn relations(app: &App, lines: &mut Vec<Line<'static>>) {
    if !target_line(app, lines) {
        return;
    }
    let Some(relations) = &app.relations else {
        return;
    };
    lines.push(field(
        app,
        Msg::LabelCurrentness,
        t(app, present::currentness(&relations.currentness)),
    ));
    lines.push(field(
        app,
        Msg::LabelResolution,
        t(app, present::resolution(&relations.target)),
    ));
    for answer in &relations.answers {
        lines.push(relation_answer(app, answer));
    }
    lines.push(Line::default());
    lines.extend(app.relations_body.iter().cloned().map(Line::from));
}

fn impact(app: &App, lines: &mut Vec<Line<'static>>) {
    lines.push(field(
        app,
        Msg::LabelChangeKind,
        format!("{}  (c)", t(app, present::change_kind(CHANGES[app.change]))),
    ));
    if !target_line(app, lines) {
        return;
    }
    if let Some(paged_answer) = &app.impact {
        paged(app, paged_answer, lines);
    }
}

fn operations(app: &App, lines: &mut Vec<Line<'static>>) {
    for (index, op) in Op::ALL.into_iter().enumerate() {
        let marker = if index == app.op_selected { "> " } else { "  " };
        lines.push(Line::from(format!(
            "{marker}{:<10} {}",
            t(app, op.msg()),
            t(app, op.description())
        )));
    }
    lines.push(Line::default());
    let Some(result) = &app.op_result else {
        return;
    };
    match result {
        OpResult::Doctor(doctor_response) => doctor(app, doctor_response, lines),
        OpResult::Sync(sync) => {
            lines.push(Line::from(t(app, Msg::ResultSynced)));
            refresh(app, &sync.refresh, lines);
            lines.push(field(
                app,
                Msg::LabelWatcher,
                watcher_text(app, &sync.watcher),
            ));
        }
        OpResult::Rebuild(rebuild) => {
            lines.push(Line::from(t(app, Msg::ResultRebuilt)));
            lines.push(field(
                app,
                Msg::LabelResources,
                rebuild.resources.to_string(),
            ));
            lines.push(field(
                app,
                Msg::LabelGeneration,
                rebuild.generation_no.to_string(),
            ));
            lines.push(field(
                app,
                Msg::LabelIndex,
                t(app, present::index(&rebuild.index)),
            ));
            lines.push(field(
                app,
                Msg::LabelWatcher,
                watcher_text(app, &rebuild.watcher),
            ));
        }
        OpResult::Uninit(uninit) => {
            lines.push(Line::from(t(
                app,
                if uninit.already_detached {
                    Msg::ResultAlreadyDetached
                } else {
                    Msg::ResultDetached
                },
            )));
            lines.push(field(app, Msg::LabelWorkspace, uninit.workspace_id.clone()));
            lines.push(field(
                app,
                Msg::ResultRuntimeStopped,
                uninit.runtime_stopped.to_string(),
            ));
        }
    }
}

fn refresh(app: &App, refresh: &PostCommandRefreshWire, lines: &mut Vec<Line<'static>>) {
    let basis = |basis: &brainprint_core::protocol::work::IndexBasisWire| {
        format!(
            "{} {} · {} {}",
            t(app, Msg::LabelRevision),
            basis.workspace_revision,
            t(app, Msg::LabelGeneration),
            basis.generation_no
        )
    };
    match refresh {
        PostCommandRefreshWire::Current {
            before,
            after,
            created_count,
            updated_count,
            deleted_count,
            ..
        } => {
            lines.push(field(
                app,
                Msg::LabelCurrentness,
                t(app, Msg::WorkspaceCurrent),
            ));
            lines.push(field(app, Msg::LabelBefore, basis(before)));
            lines.push(field(app, Msg::LabelAfter, basis(after)));
            lines.push(field(app, Msg::LabelCreated, created_count.to_string()));
            lines.push(field(app, Msg::LabelUpdated, updated_count.to_string()));
            lines.push(field(app, Msg::LabelDeleted, deleted_count.to_string()));
        }
        // Any other outcome is shown as the daemon stated it.
        other => lines.push(Line::from(format!("{other:?}"))),
    }
}

fn help(app: &App) -> Vec<Line<'static>> {
    [
        Msg::HelpTabs,
        Msg::HelpMove,
        Msg::HelpEnter,
        Msg::HelpSearch,
        Msg::HelpMore,
        Msg::HelpChange,
        Msg::HelpRefresh,
        Msg::HelpLocale,
        Msg::HelpQuit,
    ]
    .into_iter()
    .map(|msg| Line::from(t(app, msg)))
    .collect()
}

fn confirm(app: &App, op: Op) -> Vec<Line<'static>> {
    let (question, detail) = match op {
        Op::Uninit => (Msg::ConfirmUninit, Msg::ConfirmUninitDetail),
        _ => (Msg::ConfirmRebuild, Msg::ConfirmRebuildDetail),
    };
    vec![
        Line::from(t(app, question)).style(Style::default().add_modifier(Modifier::BOLD)),
        Line::from(t(app, detail)),
        Line::default(),
        Line::from(t(app, Msg::ConfirmPrompt)),
    ]
}

/// A box over the middle of the screen, as large as fits.
fn overlay(frame: &mut Frame, lines: Vec<Line<'static>>) {
    let area = frame.area();
    let width = lines
        .iter()
        .map(Line::width)
        .max()
        .unwrap_or(0)
        .saturating_add(4)
        .min(usize::from(area.width)) as u16;
    let height = (lines.len() as u16).saturating_add(2).min(area.height);
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(Block::bordered()), rect);
}
