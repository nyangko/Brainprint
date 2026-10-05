//! `brainprint doctor` / `brainprint rebuild` (#56) and `brainprint sync` /
//! `brainprint uninit` (#57), `brainprint status [path]` (#70): one
//! request each, rendered compactly.
//! `--json` prints the daemon's response as it is.

use std::io::Write as _;

use brainprint_core::protocol::{
    Request, Response,
    maintenance::{
        BackendCheckWire, BackendStateWire, CheckWire, DatabaseCheckWire, DatabaseStateWire,
        DoctorRequest, DoctorWorkspaceWire, IndexCheckWire, RebuildRequest, RuntimeCheckWire,
        SchemaCheckWire, StoredBasisWire, SyncRequest, UninitRequest, WatcherCheckWire,
        WorkspaceStatusWire,
    },
    work::{PostCommandRefreshWire, ResourceDeltaKindWire},
};

use super::exec::{Exit, absolute_workspace};
use crate::client::{self, CliError};

pub async fn run_doctor(path: Option<String>, json: bool) -> Exit {
    let path = absolute_workspace(path.as_deref().unwrap_or("."));
    let mut connection = match client::connect_and_handshake().await {
        Ok(connection) => connection,
        // The daemon/protocol check is doctor's first finding, not a
        // transport failure to hide behind a generic error.
        Err(error) => {
            println!("daemon:     {}", daemon_problem(&error));
            return Exit::DaemonOrProtocolFailure;
        }
    };
    let response =
        match client::send(&mut connection, Request::Doctor(DoctorRequest { path })).await {
            Ok(Response::Doctor(response)) => response,
            other => return unexpected("Doctor", other),
        };
    if json {
        return print_json(serde_json::to_string(&response));
    }

    let mut out = Vec::new();
    line(
        &mut out,
        "daemon",
        &format!(
            "brainprintd {} (protocol {})",
            response.daemon_version, response.protocol_version
        ),
    );
    database(&mut out, &response.global_db);
    let exit = match &response.workspace {
        DoctorWorkspaceWire::NotInitialized { path, reason } => {
            line(&mut out, "workspace", &format!("not initialized: {path}"));
            line(&mut out, "  reason", reason);
            line(&mut out, "  next", "brainprint init");
            Exit::WorkspaceConflict
        }
        DoctorWorkspaceWire::Ambiguous { path, workspaces } => {
            line(&mut out, "workspace", &format!("ambiguous: {path}"));
            line(&mut out, "  candidates", &workspaces.join(", "));
            Exit::WorkspaceConflict
        }
        DoctorWorkspaceWire::Initialized(report) => {
            line(&mut out, "workspace", &report.workspace_root);
            line(&mut out, "  project", &report.project_id);
            line(&mut out, "  workspace", &report.workspace_id);
            line(&mut out, "  binding", &check(&report.binding));
            for db in &report.databases {
                database(&mut out, db);
            }
            line(&mut out, "  index", &index(&report.index));
            runtime_lines(&mut out, &report.runtime);
            semantic(&mut out, &report.semantic);
            Exit::Ok
        }
    };
    write_out(&out, exit)
}

/// #70: `status [path]`. Without a path, the daemon's own status exactly
/// as before; with one, the Workspace's compact status beside it.
pub async fn run_status(path: Option<String>, json: bool) -> Exit {
    let path = path.map(|path| absolute_workspace(&path));
    let mut connection = match client::connect_and_handshake().await {
        Ok(connection) => connection,
        Err(error) => {
            eprintln!("brainprint: {error}");
            return Exit::DaemonOrProtocolFailure;
        }
    };
    let status = match client::status(&mut connection, path).await {
        Ok(status) => status,
        Err(error) => {
            eprintln!("brainprint: {error}");
            return Exit::DaemonOrProtocolFailure;
        }
    };
    if json {
        return print_json(serde_json::to_string(&status));
    }
    let mut out = Vec::new();
    let _ = writeln!(
        out,
        "brainprintd {} (protocol {})",
        status.daemon_version, status.protocol_version
    );
    let _ = writeln!(out, "  pid:    {}", status.pid);
    let _ = writeln!(out, "  uptime: {}s", status.uptime_seconds);
    let exit = match &status.workspace {
        None => Exit::Ok,
        Some(WorkspaceStatusWire::NotInitialized { path, reason }) => {
            line(&mut out, "workspace", &format!("not initialized: {path}"));
            line(&mut out, "  reason", reason);
            line(&mut out, "  next", "brainprint init");
            Exit::WorkspaceConflict
        }
        Some(WorkspaceStatusWire::Ambiguous { path, workspaces }) => {
            line(&mut out, "workspace", &format!("ambiguous: {path}"));
            line(&mut out, "  candidates", &workspaces.join(", "));
            Exit::WorkspaceConflict
        }
        Some(WorkspaceStatusWire::Initialized(report)) => {
            line(&mut out, "workspace", &report.workspace_root);
            line(&mut out, "  project", &report.project_id);
            line(&mut out, "  workspace", &report.workspace_id);
            match &report.basis {
                StoredBasisWire::Stable(basis) => {
                    line(&mut out, "  revision", &basis.workspace_revision);
                    line(
                        &mut out,
                        "  generation",
                        &format!(
                            "{} (basis revision {})",
                            basis.generation_no, basis.generation_basis_revision
                        ),
                    );
                    line(
                        &mut out,
                        "  incarnation",
                        &basis.index_incarnation.to_string(),
                    );
                }
                StoredBasisWire::NeverPublished => {
                    line(&mut out, "  generation", "none published yet");
                }
                StoredBasisWire::Unreadable { detail } => {
                    line(&mut out, "  generation", &format!("unreadable: {detail}"));
                }
            }
            line(&mut out, "  index", &index(&report.index));
            runtime_lines(&mut out, &report.runtime);
            semantic(&mut out, &report.semantic);
            Exit::Ok
        }
    };
    write_out(&out, exit)
}

pub async fn run_rebuild(path: Option<String>, json: bool) -> Exit {
    let path = absolute_workspace(path.as_deref().unwrap_or("."));
    let mut connection = match client::connect_and_handshake().await {
        Ok(connection) => connection,
        Err(error) => {
            eprintln!("brainprint: {error}");
            return Exit::DaemonOrProtocolFailure;
        }
    };
    let response =
        match client::send(&mut connection, Request::Rebuild(RebuildRequest { path })).await {
            Ok(Response::Rebuild(response)) => response,
            Ok(Response::Error(error)) => {
                eprintln!("brainprint: {}", error.message);
                return Exit::DaemonOrProtocolFailure;
            }
            other => return unexpected("Rebuild", other),
        };
    if json {
        return print_json(serde_json::to_string(&response));
    }

    let mut out = Vec::new();
    line(&mut out, "rebuilt", &response.workspace_root);
    line(&mut out, "  project", &response.project_id);
    line(&mut out, "  workspace", &response.workspace_id);
    line(&mut out, "  resources", &response.resources.to_string());
    line(
        &mut out,
        "  generation",
        &response.generation_no.to_string(),
    );
    line(&mut out, "  index", &index(&response.index));
    line(&mut out, "  watcher", &watcher_state(&response.watcher));
    semantic(&mut out, &response.semantic);
    write_out(&out, Exit::Ok)
}

/// Changed Resources listed by `sync` before the rest is only counted.
const SYNC_LISTED_CHANGES: usize = 20;

pub async fn run_sync(path: Option<String>, json: bool) -> Exit {
    let path = absolute_workspace(path.as_deref().unwrap_or("."));
    let mut connection = match client::connect_and_handshake().await {
        Ok(connection) => connection,
        Err(error) => {
            eprintln!("brainprint: {error}");
            return Exit::DaemonOrProtocolFailure;
        }
    };
    let response = match client::send(&mut connection, Request::Sync(SyncRequest { path })).await {
        Ok(Response::Sync(response)) => response,
        other => return unexpected("Sync", other),
    };
    if json {
        return print_json(serde_json::to_string(&response));
    }

    let mut out = Vec::new();
    line(&mut out, "synced", &response.workspace_root);
    line(&mut out, "  project", &response.project_id);
    line(&mut out, "  workspace", &response.workspace_id);
    let PostCommandRefreshWire::Current {
        before,
        after,
        created_count,
        updated_count,
        deleted_count,
        total_changed,
        changes,
        ..
    } = &response.refresh
    else {
        eprintln!("brainprint: daemon sent a sync without a current basis");
        return Exit::DaemonOrProtocolFailure;
    };
    line(
        &mut out,
        "  revision",
        &format!(
            "{} -> {}",
            before.workspace_revision, after.workspace_revision
        ),
    );
    line(
        &mut out,
        "  generation",
        &format!("{} -> {}", before.generation_no, after.generation_no),
    );
    line(
        &mut out,
        "  changes",
        &format!(
            "{total_changed} (created {created_count}, updated {updated_count}, deleted {deleted_count})"
        ),
    );
    for change in changes.iter().take(SYNC_LISTED_CHANGES) {
        let kind = match change.kind {
            ResourceDeltaKindWire::Created => "created",
            ResourceDeltaKindWire::Updated => "updated",
            ResourceDeltaKindWire::Deleted => "deleted",
        };
        let _ = writeln!(out, "    {kind:<12}{}", change.path);
    }
    let unlisted = total_changed.saturating_sub(changes.len().min(SYNC_LISTED_CHANGES) as u64);
    if unlisted > 0 {
        let _ = writeln!(
            out,
            "    ... {unlisted} more (--json lists {})",
            changes.len()
        );
    }
    line(&mut out, "  index", "current");
    line(&mut out, "  watcher", &watcher_state(&response.watcher));
    write_out(&out, Exit::Ok)
}

pub async fn run_uninit(path: Option<String>, json: bool) -> Exit {
    let path = absolute_workspace(path.as_deref().unwrap_or("."));
    let mut connection = match client::connect_and_handshake().await {
        Ok(connection) => connection,
        Err(error) => {
            eprintln!("brainprint: {error}");
            return Exit::DaemonOrProtocolFailure;
        }
    };
    let response =
        match client::send(&mut connection, Request::Uninit(UninitRequest { path })).await {
            Ok(Response::Uninit(response)) => response,
            other => return unexpected("Uninit", other),
        };
    if json {
        return print_json(serde_json::to_string(&response));
    }

    let mut out = Vec::new();
    let state = if response.already_detached {
        "already detached"
    } else {
        "detached"
    };
    line(&mut out, state, &response.workspace_root);
    line(&mut out, "  project", &response.project_id);
    line(&mut out, "  workspace", &response.workspace_id);
    line(
        &mut out,
        "  runtime",
        if response.runtime_stopped {
            "stopped (watcher detached)"
        } else {
            "none was running"
        },
    );
    line(
        &mut out,
        "  kept",
        "source, .brainprint/, durable knowledge, index",
    );
    line(&mut out, "  next", "brainprint init (attaches it again)");
    write_out(&out, Exit::Ok)
}

fn daemon_problem(error: &CliError) -> String {
    match error {
        CliError::DaemonNotRunning => "not running (start it: `brainprintd &`)".to_owned(),
        CliError::VersionMismatch {
            server_protocol_version,
            client_protocol_version,
        } => format!(
            "protocol mismatch: brainprintd {server_protocol_version}, this CLI {client_protocol_version}"
        ),
        other => other.to_string(),
    }
}

fn unexpected(operation: &str, response: Result<Response, CliError>) -> Exit {
    match response {
        Ok(Response::Error(error)) => eprintln!("brainprint: {}", error.message),
        Ok(_) => eprintln!("brainprint: daemon sent an unexpected response to {operation}"),
        Err(error) => eprintln!("brainprint: {error}"),
    }
    Exit::DaemonOrProtocolFailure
}

fn print_json(encoded: serde_json::Result<String>) -> Exit {
    let mut stdout = std::io::stdout().lock();
    let written = encoded
        .map_err(std::io::Error::from)
        .and_then(|json| writeln!(stdout, "{json}"))
        .and_then(|()| stdout.flush());
    if written.is_ok() {
        Exit::Ok
    } else {
        Exit::DaemonOrProtocolFailure
    }
}

fn write_out(out: &[u8], exit: Exit) -> Exit {
    let mut stdout = std::io::stdout().lock();
    if stdout.write_all(out).and_then(|()| stdout.flush()).is_err() {
        return Exit::DaemonOrProtocolFailure;
    }
    exit
}

fn line(out: &mut Vec<u8>, label: &str, value: &str) {
    let label = format!("{label}:");
    let _ = writeln!(out, "{label:<16}{value}");
}

fn check(check: &CheckWire) -> String {
    match check {
        CheckWire::Ok => "ok".to_owned(),
        CheckWire::Failed { detail } => format!("failed: {detail}"),
        CheckWire::NotMeasured { reason } => format!("not measured ({reason})"),
    }
}

fn database(out: &mut Vec<u8>, db: &DatabaseCheckWire) {
    let state = match &db.state {
        DatabaseStateWire::Missing => format!("missing ({})", db.path),
        DatabaseStateWire::Unreadable { detail } => format!("unreadable: {detail}"),
        DatabaseStateWire::Opened { schema, integrity } => {
            let schema = match schema {
                SchemaCheckWire::Current { version } => format!("schema {version}"),
                SchemaCheckWire::MigrationPending { version, latest } => {
                    format!("schema {version}, migration to {latest} pending")
                }
                SchemaCheckWire::Newer { version, latest } => {
                    format!("schema {version} is newer than this binary ({latest})")
                }
                SchemaCheckWire::LedgerMismatch { detail } => {
                    format!("migration ledger mismatch: {detail}")
                }
            };
            match integrity {
                CheckWire::Ok => format!("ok ({schema})"),
                other => format!("{schema}; integrity {}", self::check(other)),
            }
        }
    };
    let indent = if db.kind == "global" { "" } else { "  " };
    line(out, &format!("{indent}{}.db", db.kind), &state);
}

fn index(index: &IndexCheckWire) -> String {
    match index {
        IndexCheckWire::Current => "current".to_owned(),
        IndexCheckWire::NotCurrent { detail } => format!("not current ({detail})"),
        IndexCheckWire::NotMeasured { reason } => format!("not measured ({reason})"),
    }
}

fn runtime_lines(out: &mut Vec<u8>, runtime: &RuntimeCheckWire) {
    match runtime {
        RuntimeCheckWire::Inactive => {
            line(out, "  runtime", "inactive (the next query activates it)");
        }
        RuntimeCheckWire::Active { watcher } => {
            line(out, "  runtime", "active");
            line(out, "  watcher", &watcher_state(watcher));
        }
        RuntimeCheckWire::Unavailable { detail } => {
            line(out, "  runtime", &format!("unavailable: {detail}"));
        }
    }
}

fn watcher_state(watcher: &WatcherCheckWire) -> String {
    match watcher {
        WatcherCheckWire::Attached => "attached".to_owned(),
        WatcherCheckWire::Unavailable { reason } => {
            format!("unavailable: {reason} (currentness is proven at query time)")
        }
        WatcherCheckWire::NotStarted => "not started".to_owned(),
    }
}

fn semantic(out: &mut Vec<u8>, backends: &[BackendCheckWire]) {
    let _ = writeln!(out, "  semantic:");
    for backend in backends {
        let state = match &backend.state {
            BackendStateWire::Registered => "registered".to_owned(),
            BackendStateWire::Unavailable { reason } => format!("unavailable: {reason}"),
        };
        let _ = writeln!(out, "    {:<12}{state}", backend.family);
    }
}
