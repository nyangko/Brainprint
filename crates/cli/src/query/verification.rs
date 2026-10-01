//! `brainprint verification start|poll|cancel` (#54): a daemon-managed
//! verification Job. Each call is exactly one request: start returns once
//! the Job exists, poll returns the events after a cursor, and the caller
//! polls again when it wants more. Nothing here follows, loops or
//! retries, and the idempotency key is always the caller's.

use std::io::Write as _;

use brainprint_core::{
    VerificationJobId,
    protocol::{
        Request, Response,
        query::WorkspaceSelectorWire,
        verification_job::{
            CaptureProgressWire, MAX_POLL_EVENTS, VerificationJobCancelRequestWire,
            VerificationJobErrorWire, VerificationJobEventPayloadWire, VerificationJobEventWire,
            VerificationJobPollRequestWire, VerificationJobPollWire,
            VerificationJobStartRequestWire, VerificationJobStateWire,
        },
        work::{
            CommandCaptureWire, DiagnosticPathWire, RawAvailabilityWire, VerificationOutcomeWire,
            VerificationWire,
        },
    },
};
use clap::{Args, Subcommand};

use super::{
    exec::{Exit, absolute_workspace, exit_for_code},
    work::read_input,
};
use crate::client;

#[derive(Debug, Subcommand)]
pub enum VerificationCommand {
    /// Start a managed Job (`VerificationWire` JSON) and return at once.
    Start(StartArgs),
    /// The Job's events after `--after`, at most `--limit`.
    Poll(PollArgs),
    /// Cancel a running Job; prints its terminal state.
    Cancel(CancelArgs),
}

#[derive(Debug, Args)]
pub struct StartArgs {
    /// Reuse it to retry: the same key and input name the same Job.
    #[arg(long)]
    pub idempotency_key: String,
    /// JSON input file; `-` reads stdin.
    #[arg(long, default_value = "-")]
    pub input: String,
    #[command(flatten)]
    pub common: Common,
}

#[derive(Debug, Args)]
pub struct PollArgs {
    pub job_id: VerificationJobId,
    /// The last seq already seen (the previous poll's `next_seq`).
    #[arg(long, default_value_t = 0)]
    pub after: u64,
    #[arg(long, default_value_t = MAX_POLL_EVENTS)]
    pub limit: u32,
    #[command(flatten)]
    pub common: Common,
}

#[derive(Debug, Args)]
pub struct CancelArgs {
    pub job_id: VerificationJobId,
    #[command(flatten)]
    pub common: Common,
}

#[derive(Debug, Args)]
pub struct Common {
    /// Workspace locator, canonicalized before it crosses IPC.
    #[arg(long, default_value = ".")]
    pub workspace: String,
    /// Machine-readable output: the response JSON on stdout.
    #[arg(long)]
    pub json: bool,
}

impl Common {
    fn workspace(&self) -> WorkspaceSelectorWire {
        WorkspaceSelectorWire::Locator {
            path: absolute_workspace(&self.workspace),
        }
    }
}

pub async fn run_verification(mode: VerificationCommand) -> Exit {
    match mode {
        VerificationCommand::Start(args) => {
            let verification = read_input(&args.input).and_then(|text| {
                serde_json::from_str::<VerificationWire>(&text)
                    .map_err(|error| format!("invalid verification JSON: {error}"))
            });
            let verification = match verification {
                Ok(verification) => verification,
                Err(message) => {
                    eprintln!("brainprint: {message}");
                    return Exit::CliSyntax;
                }
            };
            let request = Request::VerificationJobStart(VerificationJobStartRequestWire {
                workspace: args.common.workspace(),
                idempotency_key: args.idempotency_key,
                verification,
            });
            let response = match send(request).await {
                Some(Response::VerificationJobStart(response)) => response,
                other => return unexpected(other),
            };
            let json = args.common.json.then(|| serde_json::to_string(&response));
            finish(&response, json, |out, started| {
                let replayed = if started.replayed { " (replayed)" } else { "" };
                writeln!(
                    out,
                    "job {} {}{replayed}",
                    started.job_id,
                    state(started.state)
                )
            })
        }
        VerificationCommand::Poll(args) => {
            let request = Request::VerificationJobPoll(VerificationJobPollRequestWire {
                workspace: args.common.workspace(),
                job_id: args.job_id,
                after_seq: args.after,
                limit: args.limit,
            });
            let response = match send(request).await {
                Some(Response::VerificationJobPoll(response)) => response,
                other => return unexpected(other),
            };
            let json = args.common.json.then(|| serde_json::to_string(&response));
            finish(&response, json, print_poll)
        }
        VerificationCommand::Cancel(args) => {
            let request = Request::VerificationJobCancel(VerificationJobCancelRequestWire {
                workspace: args.common.workspace(),
                job_id: args.job_id,
            });
            let response = match send(request).await {
                Some(Response::VerificationJobCancel(response)) => response,
                other => return unexpected(other),
            };
            let json = args.common.json.then(|| serde_json::to_string(&response));
            finish(&response, json, |out, cancelled| {
                writeln!(out, "job {} {}", cancelled.job_id, state(cancelled.state))
            })
        }
    }
}

/// One request on a fresh connection; `None` once the failure is told.
async fn send(request: Request) -> Option<Response> {
    let sent = match client::connect_and_handshake().await {
        Ok(mut connection) => client::send(&mut connection, request).await,
        Err(error) => Err(error),
    };
    match sent {
        Ok(Response::Error(error)) => {
            eprintln!("brainprint: {}", error.message);
            None
        }
        Ok(response) => Some(response),
        Err(error) => {
            eprintln!("brainprint: {error}");
            None
        }
    }
}

/// `None` was already told by [`send`].
fn unexpected(response: Option<Response>) -> Exit {
    if response.is_some() {
        eprintln!("brainprint: daemon sent an unexpected response");
    }
    Exit::DaemonOrProtocolFailure
}

/// Prints the response -- its `json` when given, else `compact` of a
/// success -- and maps it to the exit code. Both say the same facts.
fn finish<T>(
    response: &Result<T, VerificationJobErrorWire>,
    json: Option<serde_json::Result<String>>,
    compact: impl FnOnce(&mut std::io::StdoutLock<'_>, &T) -> std::io::Result<()>,
) -> Exit {
    let mut out = std::io::stdout().lock();
    let printed = match (json, response) {
        (Some(json), _) => json
            .map_err(std::io::Error::from)
            .and_then(|json| writeln!(out, "{json}")),
        (None, Ok(value)) => compact(&mut out, value),
        (None, Err(_)) => Ok(()),
    };
    if printed.and_then(|()| out.flush()).is_err() {
        return Exit::DaemonOrProtocolFailure;
    }
    match response {
        Ok(_) => Exit::Ok,
        Err(error) => {
            eprintln!("brainprint: {}", describe(error));
            exit_for(error)
        }
    }
}

fn exit_for(error: &VerificationJobErrorWire) -> Exit {
    use VerificationJobErrorWire as E;
    match error {
        E::Workspace(error) => exit_for_code(error.code),
        E::InvalidVerification { .. } | E::InvalidIdempotencyKey | E::InvalidLimit => {
            Exit::CliSyntax
        }
        E::VerificationBusy | E::IdempotencyConflict | E::JobNotFound => {
            Exit::QueryOrDeliveryFailure
        }
        E::Corrupt | E::Internal { .. } => Exit::DaemonOrProtocolFailure,
    }
}

fn describe(error: &VerificationJobErrorWire) -> String {
    use VerificationJobErrorWire as E;
    match error {
        E::Workspace(error) => format!("{:?}: {}", error.code, error.message),
        E::InvalidVerification { reason } => format!("invalid verification: {reason}"),
        E::InvalidIdempotencyKey => "invalid --idempotency-key".to_owned(),
        E::IdempotencyConflict => {
            "the idempotency key names a Job with a different request; nothing ran".to_owned()
        }
        E::VerificationBusy => "a verification is already running for this Workspace or the daemon is at its limit; no Job was created, retry later".to_owned(),
        E::JobNotFound => "no such verification Job in this Workspace".to_owned(),
        E::InvalidLimit => format!("--limit must be 1..={MAX_POLL_EVENTS}"),
        E::Corrupt => "a stored verification Job is corrupt".to_owned(),
        E::Internal { message } => message.clone(),
    }
}

fn state(state: VerificationJobStateWire) -> &'static str {
    match state {
        VerificationJobStateWire::Running => "running",
        VerificationJobStateWire::Finished => "finished",
        VerificationJobStateWire::Cancelled => "cancelled",
        VerificationJobStateWire::Interrupted => "interrupted",
        VerificationJobStateWire::InternalError => "internal_error",
    }
}

/// The new events, one line each, then the cursor for the next poll.
fn print_poll(
    out: &mut std::io::StdoutLock<'_>,
    poll: &VerificationJobPollWire,
) -> std::io::Result<()> {
    for event in &poll.events {
        print_event(out, event)?;
    }
    let more = if poll.has_more { " (more)" } else { "" };
    writeln!(
        out,
        "job {} {} next {}{more}",
        poll.job_id,
        state(poll.state),
        poll.next_seq
    )
}

fn print_event(
    out: &mut std::io::StdoutLock<'_>,
    event: &VerificationJobEventWire,
) -> std::io::Result<()> {
    use VerificationJobEventPayloadWire as P;
    let seq = event.seq;
    match &event.payload {
        P::JobStarted => writeln!(out, "[{seq}] job started"),
        P::CommandStarted { label, .. } => writeln!(out, "[{seq}] {label} started"),
        P::CommandFinished(finished) => {
            let label = &finished.label;
            let ms = finished.duration_ms;
            match finished.outcome {
                VerificationOutcomeWire::Passed => write!(out, "[{seq}] {label} passed {ms}ms")?,
                VerificationOutcomeWire::Failed { exit_code } => {
                    write!(out, "[{seq}] {label} failed exit={exit_code} {ms}ms")?;
                }
                VerificationOutcomeWire::Signaled { signal } => {
                    write!(out, "[{seq}] {label} signaled signal={signal} {ms}ms")?;
                }
                VerificationOutcomeWire::TimedOut => {
                    write!(out, "[{seq}] {label} timed out {ms}ms")?;
                }
                VerificationOutcomeWire::NotStarted { reason } => {
                    write!(out, "[{seq}] {label} not started {reason:?}")?;
                }
                VerificationOutcomeWire::Skipped => write!(out, "[{seq}] {label} skipped")?,
            }
            if let CaptureProgressWire::Captured {
                raw: RawAvailabilityWire::Available(raw),
                ..
            } = &finished.capture
            {
                write!(out, " raw={}", raw.handle)?;
            }
            writeln!(out)
        }
        P::JobFinished { results, .. } => {
            writeln!(out, "[{seq}] job finished")?;
            // #53 already bounds these; never the raw output.
            for result in results {
                let CommandCaptureWire::Captured {
                    diagnostics: Some(summary),
                    ..
                } = &result.capture
                else {
                    continue;
                };
                for item in &summary.items {
                    let path = match &item.path {
                        DiagnosticPathWire::Workspace { path } => path.as_str(),
                        DiagnosticPathWire::Absent => "-",
                        DiagnosticPathWire::External => "<external>",
                        DiagnosticPathWire::Unresolved => "<unresolved>",
                    };
                    let line = item.line.map_or(String::new(), |line| format!(":{line}"));
                    let column = item
                        .column
                        .map_or(String::new(), |column| format!(":{column}"));
                    let message = item.message.lines().next().unwrap_or_default();
                    writeln!(
                        out,
                        "    {} {:?} {path}{line}{column} {message}",
                        result.label, item.severity
                    )?;
                }
                let left_out = summary.omitted + summary.delivery_omitted;
                if left_out > 0 {
                    writeln!(out, "    {} {left_out} more diagnostics", result.label)?;
                }
            }
            Ok(())
        }
        P::JobCancelled { .. } => writeln!(out, "[{seq}] job cancelled"),
        P::JobInterrupted { reason } => writeln!(out, "[{seq}] job interrupted {reason:?}"),
        P::JobInternalError { reason } => writeln!(out, "[{seq}] job internal error {reason:?}"),
    }
}
