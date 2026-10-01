//! `brainprint artifact read` (#53): one bounded read of an ephemeral raw
//! artifact a verification `capture` produced. Exactly one request: the
//! caller continues from `next_offset` itself; nothing is paged or
//! retried here.

use std::io::Write as _;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use brainprint_core::protocol::{
    Request, Response,
    artifact::{
        ArtifactPartWire, ArtifactReadErrorWire, ArtifactReadRequestWire, ArtifactReadResponseWire,
        MAX_ARTIFACT_READ_BYTES,
    },
    work::OutputStreamWire,
};
use clap::{Args, Subcommand, ValueEnum};

use super::exec::Exit;
use crate::client;

#[derive(Debug, Subcommand)]
pub enum ArtifactCommand {
    /// Read one chunk of a retained head or tail. Without `--json` the
    /// raw bytes are written to stdout exactly as captured.
    Read(ArtifactReadArgs),
}

#[derive(Debug, Args)]
pub struct ArtifactReadArgs {
    /// The opaque handle from a Work response's raw reference.
    pub handle: String,
    #[arg(long)]
    pub stream: StreamArg,
    #[arg(long)]
    pub part: PartArg,
    /// Offset within the chosen part.
    #[arg(long, default_value_t = 0)]
    pub offset: u32,
    #[arg(
        long,
        default_value_t = MAX_ARTIFACT_READ_BYTES,
        value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_ARTIFACT_READ_BYTES)),
    )]
    pub max_bytes: u32,
    /// Machine-readable output: the `ArtifactRead` response JSON (base64
    /// data) on stdout.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum StreamArg {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PartArg {
    Head,
    Tail,
}

pub async fn run_artifact(mode: ArtifactCommand) -> Exit {
    let ArtifactCommand::Read(args) = mode;
    let mut connection = match client::connect_and_handshake().await {
        Ok(connection) => connection,
        Err(error) => {
            eprintln!("brainprint: {error}");
            return Exit::DaemonOrProtocolFailure;
        }
    };
    let request = Request::ArtifactRead(ArtifactReadRequestWire {
        handle: args.handle,
        stream: match args.stream {
            StreamArg::Stdout => OutputStreamWire::Stdout,
            StreamArg::Stderr => OutputStreamWire::Stderr,
        },
        part: match args.part {
            PartArg::Head => ArtifactPartWire::Head,
            PartArg::Tail => ArtifactPartWire::Tail,
        },
        offset: args.offset,
        max_bytes: args.max_bytes,
    });
    let response = match client::send(&mut connection, request).await {
        Ok(Response::ArtifactRead(response)) => response,
        Ok(_) => {
            eprintln!("brainprint: daemon sent an unexpected response to ArtifactRead");
            return Exit::DaemonOrProtocolFailure;
        }
        Err(error) => {
            eprintln!("brainprint: {error}");
            return Exit::DaemonOrProtocolFailure;
        }
    };

    let mut stdout = std::io::stdout().lock();
    let written = match &response {
        _ if args.json => serde_json::to_writer(&mut stdout, &response)
            .map_err(std::io::Error::from)
            .and_then(|()| writeln!(stdout)),
        ArtifactReadResponseWire::Data { data_b64, .. } => match STANDARD.decode(data_b64) {
            Ok(bytes) => stdout.write_all(&bytes),
            Err(_) => {
                eprintln!("brainprint: daemon sent invalid base64");
                return Exit::DaemonOrProtocolFailure;
            }
        },
        ArtifactReadResponseWire::Failed { .. } => Ok(()),
    };
    if written.and_then(|()| stdout.flush()).is_err() {
        return Exit::DaemonOrProtocolFailure;
    }
    match response {
        ArtifactReadResponseWire::Data { .. } => Exit::Ok,
        ArtifactReadResponseWire::Failed { error } => {
            let (message, exit) = match error {
                ArtifactReadErrorWire::ArtifactUnavailable => (
                    "artifact unavailable (unknown, evicted, or from before a daemon restart)"
                        .to_owned(),
                    Exit::QueryOrDeliveryFailure,
                ),
                ArtifactReadErrorWire::InvalidRequest { reason } => {
                    (format!("invalid request: {reason}"), Exit::CliSyntax)
                }
                ArtifactReadErrorWire::Internal => (
                    "the daemon could not read the artifact".to_owned(),
                    Exit::DaemonOrProtocolFailure,
                ),
            };
            eprintln!("brainprint: {message}");
            exit
        }
    }
}
