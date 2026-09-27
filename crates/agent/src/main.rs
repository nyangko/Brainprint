//! `brainprint-agent` entrypoint (#26 "Command surface").
//!
//! Hook commands (`event`, `bridge`) fail open: any parse/runtime/daemon
//! failure prints the client's no-op response, exits 0, and reports the
//! problem on stderr only -- never on protocol stdout.

use std::{
    io::{self, Read as _, Write as _},
    process::{Command, ExitCode, Stdio},
    time::Instant,
};

use brainprint_agent::{
    clients::ClientId,
    event::{
        ClientCapabilities, EventKind, FallbackReason, IntegrationEvent, Mode, NativeAction,
        ResetSource,
    },
    gateway,
    probe::{IpcProbe, Probe as _},
    state::StateStore,
    telemetry::{self, TelemetryEvent},
    util,
};
use brainprint_core::protocol::EndpointPaths;
use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "brainprint-agent",
    version,
    about = "Brainprint Integration Gateway + thin client signal bridges"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Common normalized path: a normalized IntegrationEvent JSON on stdin.
    Event {
        #[arg(long)]
        kind: String,
        #[arg(long, value_enum)]
        mode: Mode,
    },
    /// Thin native payload translation: a client's hook JSON on stdin.
    Bridge {
        #[arg(long, value_enum)]
        client: ClientId,
        #[arg(long)]
        event: String,
        #[arg(long, value_enum)]
        mode: Mode,
        /// Optional, recorded in telemetry only.
        #[arg(long)]
        client_version: Option<String>,
    },
    /// Report the installed client's executable/version and the
    /// capability matrix this bridge maps.
    Probe {
        #[arg(long, value_enum)]
        client: ClientId,
    },
    /// Print a deterministic integration fragment. Never edits config.
    Config {
        #[arg(long, value_enum)]
        client: ClientId,
        #[arg(long, value_enum)]
        mode: Mode,
        /// Command the hooks invoke (default: `brainprint-agent`).
        #[arg(long, default_value = "brainprint-agent")]
        agent_command: String,
    },
    /// Allow exactly one next native exploration in guard mode.
    BypassOnce {
        #[arg(long)]
        session: String,
        /// Limit to one client's state (default: every client that has
        /// this session).
        #[arg(long, value_enum)]
        client: Option<ClientId>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Commands::Event { kind, mode } => {
            normalized_event(&kind, mode);
            ExitCode::SUCCESS
        }
        Commands::Bridge {
            client,
            event,
            mode,
            client_version,
        } => {
            bridge(client, &event, mode, client_version);
            ExitCode::SUCCESS
        }
        Commands::Probe { client } => {
            println!("{}", probe_client(client));
            ExitCode::SUCCESS
        }
        Commands::Config {
            client,
            mode,
            agent_command,
        } => {
            let fragment = client.config(mode, &agent_command);
            println!(
                "{}",
                serde_json::to_string_pretty(&fragment).unwrap_or_default()
            );
            ExitCode::SUCCESS
        }
        Commands::BypassOnce { session, client } => bypass_once(&session, client),
    }
}

/// The wire shape of `brainprint-agent event` input.
#[derive(Deserialize)]
struct NormalizedInput {
    client_id: String,
    #[serde(default)]
    client_version: Option<String>,
    session_id: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    reset_source: Option<ResetSource>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    can_inject_context: bool,
    #[serde(default)]
    action: Option<NativeAction>,
    #[serde(default)]
    capabilities: ClientCapabilities,
}

fn normalized_event(kind: &str, mode: Mode) {
    let started = Instant::now();
    let parsed = EventKind::parse(kind)
        .ok_or_else(|| format!("unknown normalized event kind {kind}"))
        .and_then(|kind| {
            let input: NormalizedInput = serde_json::from_str(&read_stdin()?)
                .map_err(|error| format!("invalid normalized event: {error}"))?;
            Ok(IntegrationEvent {
                kind,
                client_id: input.client_id,
                client_version: input.client_version,
                session_id: input.session_id,
                cwd: input.cwd,
                reset_source: input.reset_source,
                agent_id: input.agent_id,
                can_inject_context: input.can_inject_context,
                action: input.action,
                capabilities: input.capabilities,
            })
        });
    let fail_open = |reason: &str| {
        eprintln!("brainprint-agent: {reason}; allowing native fallback");
        println!(
            "{}",
            json!({ "decision": "allow", "fallback_reason": FallbackReason::BridgeError })
        );
    };
    let event = match parsed {
        Ok(event) => event,
        Err(reason) => return fail_open(&reason),
    };
    let Some(runtime_root) = runtime_root() else {
        return fail_open("could not resolve the Brainprint runtime root");
    };
    let store = StateStore::new(&runtime_root, &event.client_id);
    let mut probe = IpcProbe::new(None);
    let outcome = gateway::handle(&event, mode, &store, &mut probe);
    let latency = util::micros(started.elapsed());
    emit_telemetry(&event, mode, &outcome, probe.stats(), latency);
    println!(
        "{}",
        json!({
            "decision": outcome.decision.kind,
            "message": outcome.decision.message,
            "bootstrap": outcome.decision.bootstrap,
            "fallback_reason": outcome.decision.fallback,
            "exact_substitute_proven": outcome.exact_substitute_proven,
            "route": outcome.route,
            "daemon_probe_count": probe.stats().count,
            "latency_us": latency,
        })
    );
}

fn bridge(client: ClientId, native_event: &str, mode: Mode, client_version: Option<String>) {
    let started = Instant::now();
    let result = read_stdin()
        .and_then(|text| {
            serde_json::from_str(&text).map_err(|error| format!("invalid hook JSON: {error}"))
        })
        .and_then(|payload| client.normalize(native_event, &payload, client_version));
    let event = match result {
        Ok(event) => event,
        Err(reason) => {
            eprintln!("brainprint-agent: {reason}; allowing native fallback");
            return print_rendered(&client.fail_open());
        }
    };
    let Some(runtime_root) = runtime_root() else {
        eprintln!(
            "brainprint-agent: could not resolve the Brainprint runtime root; allowing native fallback"
        );
        return print_rendered(&client.fail_open());
    };
    let store = StateStore::new(&runtime_root, client.id());
    let mut probe = IpcProbe::new(None);
    let outcome = gateway::handle(&event, mode, &store, &mut probe);
    let rendered = client.render(native_event, &outcome.decision);
    print_rendered(&rendered);
    emit_telemetry(
        &event,
        mode,
        &outcome,
        probe.stats(),
        util::micros(started.elapsed()),
    );
}

fn print_rendered(rendered: &brainprint_agent::clients::Rendered) {
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(rendered.stdout.as_bytes());
    let _ = stdout.flush();
    if rendered.exit_code != 0 {
        std::process::exit(rendered.exit_code);
    }
}

fn emit_telemetry(
    event: &IntegrationEvent,
    mode: Mode,
    outcome: &gateway::Outcome,
    probes: brainprint_agent::probe::ProbeStats,
    latency_us: u64,
) {
    if !outcome.observed {
        return;
    }
    let Some(path) = telemetry::sink_path() else {
        return;
    };
    let line = TelemetryEvent::new(event, mode, outcome, probes, latency_us);
    if let Err(error) = telemetry::append(&path, &line) {
        eprintln!("brainprint-agent: telemetry append failed: {error}");
    }
}

fn read_stdin() -> Result<String, String> {
    let mut text = String::new();
    io::stdin()
        .read_to_string(&mut text)
        .map_err(|error| format!("could not read stdin: {error}"))?;
    Ok(text)
}

fn runtime_root() -> Option<std::path::PathBuf> {
    EndpointPaths::resolve()
        .ok()
        .map(|endpoint| endpoint.runtime_root)
}

fn probe_client(client: ClientId) -> serde_json::Value {
    let output = Command::new(client.binary())
        .arg("--version")
        .stdin(Stdio::null())
        .output();
    let (executable_found, version) = match output {
        Ok(output) if output.status.success() => (
            true,
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .map(|line| line.trim().to_owned()),
        ),
        Ok(_) => (true, None),
        Err(_) => (false, None),
    };
    json!({
        "client": client.id(),
        "executable": client.binary(),
        "executable_found": executable_found,
        "version": version,
        "capabilities": client.capabilities(),
        "events": client
            .event_map()
            .iter()
            .map(|(native, normalized)| json!({ "native": native, "normalized": normalized }))
            .collect::<Vec<_>>(),
    })
}

fn bypass_once(session: &str, client: Option<ClientId>) -> ExitCode {
    let Some(runtime_root) = runtime_root() else {
        eprintln!("brainprint-agent: could not resolve the Brainprint runtime root");
        return ExitCode::FAILURE;
    };
    let candidates = match client {
        Some(client) => vec![client],
        None => vec![
            ClientId::ClaudeCode,
            ClientId::GeminiCli,
            ClientId::CodexCli,
        ],
    };
    let mut armed = Vec::new();
    for candidate in candidates {
        let store = StateStore::new(&runtime_root, candidate.id());
        // Without --client, arm only clients that already know the session.
        if client.is_none() && !store.exists(session) {
            continue;
        }
        let mut record = store.load(session);
        record.bypass = true;
        if store.save(session, &mut record).is_ok() {
            armed.push(candidate.id());
        }
    }
    println!("{}", json!({ "session": session, "armed": armed }));
    if armed.is_empty() {
        eprintln!(
            "brainprint-agent: no adapter state for session {session}; pass --client to arm it anyway"
        );
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
