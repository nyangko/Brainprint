//! `brainprint work start|result` (#50, I6 task 1): send a caller-observed
//! Git state to the Working State lifecycle. The input is one JSON
//! document (`WorkStartWire` / `WorkResultInputWire`) from a file or
//! stdin; this command runs no Git and parses no porcelain text. #52: a
//! Result's `verification` is run by the daemon; its per-command results
//! are shown, and a recorded result exits 0 whatever they were.

use std::io::{Read as _, Write as _};

use brainprint_core::protocol::{
    Request, Response,
    query::{DirtyObservationWire, WorkspaceSelectorWire},
    work::{
        CommandResultWire, WorkErrorWire, WorkOperationWire, WorkRequest, WorkResponse,
        WorkResultInputWire, WorkStartWire,
    },
};
use clap::{Args, Subcommand};

use super::exec::{Exit, absolute_workspace, exit_for_code};
use crate::client;

#[derive(Debug, Subcommand)]
pub enum WorkCommand {
    /// OPEN → ACTIVE with the observed baseline (`WorkStartWire` JSON).
    Start(WorkArgs),
    /// Record a partial/complete/abandon result (`WorkResultInputWire`
    /// JSON).
    Result(WorkArgs),
}

#[derive(Debug, Args)]
pub struct WorkArgs {
    /// JSON input file; `-` reads stdin.
    #[arg(long, default_value = "-")]
    pub input: String,
    /// Workspace locator, canonicalized before it crosses IPC.
    #[arg(long, default_value = ".")]
    pub workspace: String,
    /// Machine-readable output: one `WorkResponse` JSON on stdout.
    #[arg(long)]
    pub json: bool,
}

pub async fn run_work(mode: WorkCommand) -> Exit {
    let (args, operation) = match mode {
        WorkCommand::Start(args) => {
            let operation = read_input(&args.input).and_then(|text| {
                serde_json::from_str::<WorkStartWire>(&text)
                    .map(WorkOperationWire::Start)
                    .map_err(invalid_json)
            });
            (args, operation)
        }
        WorkCommand::Result(args) => {
            let operation = read_input(&args.input).and_then(|text| {
                serde_json::from_str::<WorkResultInputWire>(&text)
                    .map(WorkOperationWire::Result)
                    .map_err(invalid_json)
            });
            (args, operation)
        }
    };
    let operation = match operation {
        Ok(operation) => operation,
        Err(message) => {
            eprintln!("brainprint: {message}");
            return Exit::CliSyntax;
        }
    };

    let mut connection = match client::connect_and_handshake().await {
        Ok(connection) => connection,
        Err(error) => {
            eprintln!("brainprint: {error}");
            return Exit::DaemonOrProtocolFailure;
        }
    };
    let request = Request::Work(WorkRequest {
        workspace: WorkspaceSelectorWire::Locator {
            path: absolute_workspace(&args.workspace),
        },
        operation,
    });
    let response = match client::send(&mut connection, request).await {
        Ok(Response::Work(response)) => response,
        Ok(_) => {
            eprintln!("brainprint: daemon sent an unexpected response to Work");
            return Exit::DaemonOrProtocolFailure;
        }
        Err(error) => {
            eprintln!("brainprint: {error}");
            return Exit::DaemonOrProtocolFailure;
        }
    };

    let printed = if args.json {
        print_json(&response)
    } else {
        print_compact(&response)
    };
    if printed.is_err() {
        return Exit::DaemonOrProtocolFailure;
    }
    match &response {
        WorkResponse::Started(_) | WorkResponse::Recorded(_) => Exit::Ok,
        WorkResponse::Failed(failure) => {
            eprintln!("brainprint: {}", describe(&failure.error));
            if let Some(created) = failure.created_work_item {
                eprintln!(
                    "brainprint: WorkItem {created} was created and is OPEN; retry start with it as Existing"
                );
            }
            exit_for(&failure.error)
        }
    }
}

fn read_input(input: &str) -> Result<String, String> {
    let mut text = String::new();
    let read = if input == "-" {
        std::io::stdin().read_to_string(&mut text)
    } else {
        std::fs::File::open(input).and_then(|mut file| file.read_to_string(&mut text))
    };
    read.map(|_| text)
        .map_err(|error| format!("cannot read --input {input}: {error}"))
}

fn invalid_json(error: serde_json::Error) -> String {
    format!("invalid work JSON: {error}")
}

fn exit_for(error: &WorkErrorWire) -> Exit {
    match error {
        WorkErrorWire::Workspace(error) => exit_for_code(error.code),
        WorkErrorWire::InvalidObservation { .. } | WorkErrorWire::BoundExceeded { .. } => {
            Exit::CliSyntax
        }
        WorkErrorWire::WorkItemNotFound { .. }
        | WorkErrorWire::InvalidTransition { .. }
        | WorkErrorWire::WorkspaceNotReady(_)
        | WorkErrorWire::GitObservation(_)
        | WorkErrorWire::VerificationBusy => Exit::QueryOrDeliveryFailure,
        WorkErrorWire::Internal { .. } => Exit::DaemonOrProtocolFailure,
    }
}

fn describe(error: &WorkErrorWire) -> String {
    match error {
        WorkErrorWire::Workspace(error) => format!("{:?}: {}", error.code, error.message),
        WorkErrorWire::InvalidObservation { reason } => format!("invalid observation: {reason}"),
        WorkErrorWire::BoundExceeded { what } => format!("{what} exceed the bound"),
        WorkErrorWire::WorkItemNotFound { work_item } => {
            format!("WorkItem {work_item} is not in this Workspace")
        }
        WorkErrorWire::InvalidTransition { reason } => format!("invalid transition: {reason}"),
        WorkErrorWire::WorkspaceNotReady(reason) => {
            format!("Workspace cannot take a baseline yet ({reason:?}); nothing was written, retry")
        }
        WorkErrorWire::GitObservation(reason) => format!(
            "Git observation failed ({reason:?}); nothing was written. Retry, or send an explicit \"Unknown\""
        ),
        WorkErrorWire::VerificationBusy => {
            "a verification is already running for this Workspace or the daemon is at its limit; nothing ran or was written, retry later".to_owned()
        }
        WorkErrorWire::Internal { message } => message.clone(),
    }
}

fn print_json(response: &WorkResponse) -> std::io::Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, response)?;
    writeln!(stdout)?;
    stdout.flush()
}

fn dirty(observation: &DirtyObservationWire) -> String {
    match observation {
        DirtyObservationWire::Unknown => "UNKNOWN".to_owned(),
        DirtyObservationWire::Clean => "CLEAN".to_owned(),
        DirtyObservationWire::Dirty { fingerprint } => format!("DIRTY {fingerprint}"),
    }
}

fn print_compact(response: &WorkResponse) -> std::io::Result<()> {
    let mut stdout = std::io::stdout().lock();
    match response {
        WorkResponse::Started(started) => {
            let state = &started.working_state;
            writeln!(stdout, "started {}", state.work_item)?;
            writeln!(
                stdout,
                "  baseline: revision {} generation {}",
                state.baseline_workspace_revision, state.baseline_generation_no
            )?;
            if let Some(head) = &state.baseline_head {
                writeln!(stdout, "  head: {head}")?;
            }
            writeln!(stdout, "  dirty: {}", dirty(&state.baseline_dirty))?;
            for path in &started.unresolved_paths {
                writeln!(stdout, "  unresolved: {path}")?;
            }
        }
        WorkResponse::Recorded(recorded) => {
            let result = &recorded.result;
            writeln!(
                stdout,
                "recorded {:?} for {}: status {:?}",
                result.result_status, result.work_item, recorded.status
            )?;
            writeln!(
                stdout,
                "  remaining dirty: {}",
                dirty(&result.remaining_dirty)
            )?;
            if let Some(fingerprint) = &result.change_set_fingerprint {
                writeln!(stdout, "  change set: {fingerprint}")?;
            }
            print_verification(&mut stdout, recorded.verification.as_deref())?;
        }
        WorkResponse::Failed(failure) => {
            if failure.verification.is_some() {
                writeln!(stdout, "not recorded; the verification ran:")?;
            }
            print_verification(&mut stdout, failure.verification.as_deref())?;
        }
    }
    stdout.flush()
}

fn print_verification(
    stdout: &mut impl std::io::Write,
    results: Option<&[CommandResultWire]>,
) -> std::io::Result<()> {
    for result in results.unwrap_or_default() {
        writeln!(
            stdout,
            "  verification {}: {:?} {}ms stdout {}B stderr {}B",
            result.label,
            result.outcome,
            result.duration_ms,
            result.stdout_bytes,
            result.stderr_bytes
        )?;
    }
    Ok(())
}
