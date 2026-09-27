//! Thin Signal Bridges (#30 "Thin Signal Bridge boundary"): native
//! event/payload -> [`IntegrationEvent`], and common [`Decision`] ->
//! native hook response. No freshness, substitution, Policy or Working
//! State logic lives here -- a bridge only parses, translates, formats
//! and prints configuration.

pub mod claude;
pub mod codex;
pub mod gemini;

use serde_json::Value;

use crate::{
    event::{
        ActionClass, BrainprintTool, ClientCapabilities, Decision, FallbackReason,
        IntegrationEvent, Mode, NativeAction,
    },
    shell,
};

/// The P0 reference bridges. Adding a client with equivalent
/// capabilities means adding one of these, never new gateway logic
/// (#26 acceptance 49).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ClientId {
    ClaudeCode,
    GeminiCli,
    CodexCli,
}

/// A native hook response: exact stdout, exit code, optional stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub stdout: String,
    pub exit_code: i32,
}

impl ClientId {
    pub const fn id(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::GeminiCli => "gemini-cli",
            Self::CodexCli => "codex-cli",
        }
    }

    pub const fn binary(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::GeminiCli => "gemini",
            Self::CodexCli => "codex",
        }
    }

    pub const fn capabilities(self) -> ClientCapabilities {
        match self {
            Self::ClaudeCode => claude::CAPABILITIES,
            Self::GeminiCli => gemini::CAPABILITIES,
            Self::CodexCli => codex::CAPABILITIES,
        }
    }

    /// Native event names this bridge translates, with their normalized
    /// kind -- the capability matrix `probe` reports.
    pub const fn event_map(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::ClaudeCode => claude::EVENTS,
            Self::GeminiCli => gemini::EVENTS,
            Self::CodexCli => codex::EVENTS,
        }
    }

    pub fn normalize(
        self,
        native_event: &str,
        payload: &Value,
        client_version: Option<String>,
    ) -> Result<IntegrationEvent, String> {
        let mut event = match self {
            Self::ClaudeCode => claude::normalize(native_event, payload)?,
            Self::GeminiCli => gemini::normalize(native_event, payload)?,
            Self::CodexCli => codex::normalize(native_event, payload)?,
        };
        event.client_version = client_version;
        Ok(event)
    }

    pub fn render(self, native_event: &str, decision: &Decision) -> Rendered {
        match self {
            Self::ClaudeCode | Self::CodexCli => claude_wire_render(native_event, decision),
            Self::GeminiCli => gemini::render(native_event, decision),
        }
    }

    /// The response that changes nothing: used on any bridge failure.
    pub fn fail_open(self) -> Rendered {
        match self {
            Self::ClaudeCode | Self::CodexCli => Rendered {
                stdout: String::new(),
                exit_code: 0,
            },
            Self::GeminiCli => Rendered {
                stdout: "{}".to_owned(),
                exit_code: 0,
            },
        }
    }

    pub fn config(self, mode: Mode, agent_command: &str) -> Value {
        match self {
            Self::ClaudeCode => claude::config(mode, agent_command),
            Self::GeminiCli => gemini::config(mode, agent_command),
            Self::CodexCli => codex::config(mode, agent_command),
        }
    }
}

pub fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Observe => "observe",
        Mode::Prefer => "prefer",
        Mode::Guard => "guard",
    }
}

/// Shared by Claude Code and Codex CLI, whose hook response wire is the
/// same shape (`hookSpecificOutput` + `permissionDecision`). Never
/// emits `allow`: a hook allow would skip the client's own permission
/// flow, and this integration is not a permission system (#26 "Failure
/// policy").
pub fn claude_wire_render(native_event: &str, decision: &Decision) -> Rendered {
    use crate::event::DecisionKind;

    let mut output = serde_json::Map::new();
    output.insert("hookEventName".into(), native_event.into());
    if native_event == "PreToolUse" && decision.kind == DecisionKind::SuppressRedirect {
        output.insert("permissionDecision".into(), "deny".into());
        output.insert(
            "permissionDecisionReason".into(),
            decision.message.clone().unwrap_or_default().into(),
        );
        if let Some(bootstrap) = decision.bootstrap {
            output.insert("additionalContext".into(), bootstrap.into());
        }
    } else if let Some(context) = decision.context_text() {
        output.insert("additionalContext".into(), context.into());
    } else {
        return Rendered {
            stdout: String::new(),
            exit_code: 0,
        };
    }
    Rendered {
        stdout: serde_json::json!({ "hookSpecificOutput": output }).to_string(),
        exit_code: 0,
    }
}

pub fn str_field<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

pub fn session_id(payload: &Value) -> Result<String, String> {
    str_field(payload, "session_id")
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "hook payload has no session_id".to_owned())
}

/// Keys present in a tool input beyond `allowed`.
pub fn has_unknown_keys(input: &Value, allowed: &[&str]) -> bool {
    input
        .as_object()
        .is_some_and(|map| map.keys().any(|key| !allowed.contains(&key.as_str())))
}

pub fn usize_field(input: &Value, key: &str) -> Option<usize> {
    input
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

/// A shell command, given as a string or an argv (unwrapping a plain
/// `sh|bash|zsh -c|-lc <command>`). Anything else is opaque.
pub fn shell_action(command: &Value) -> NativeAction {
    match command {
        Value::String(text) => shell::classify(text),
        Value::Array(argv) => {
            let parts: Option<Vec<&str>> = argv.iter().map(Value::as_str).collect();
            match parts.as_deref() {
                Some([shell_name, flag, script])
                    if matches!(
                        *shell_name,
                        "sh" | "bash" | "zsh" | "/bin/sh" | "/bin/bash" | "/bin/zsh"
                    ) && matches!(*flag, "-c" | "-lc") =>
                {
                    shell::classify(script)
                }
                _ => NativeAction::Opaque,
            }
        }
        _ => NativeAction::Opaque,
    }
}

pub fn brainprint_action(
    tool_name: &str,
    input: &Value,
    result: Option<&Value>,
) -> Option<NativeAction> {
    BrainprintTool::from_tool_name(tool_name).map(|tool| NativeAction::Brainprint {
        tool,
        input: input.clone(),
        result: result.cloned(),
    })
}

pub const fn unproven(class: ActionClass, reason: FallbackReason) -> NativeAction {
    NativeAction::UnprovenExploration { class, reason }
}

/// A glob that means exactly "every file, recursively".
pub fn is_all_files_glob(pattern: &str) -> bool {
    matches!(pattern, "**/*" | "**")
}

pub fn hook_command(
    agent_command: &str,
    client: ClientId,
    native_event: &str,
    mode: Mode,
) -> String {
    format!(
        "{agent_command} bridge --client {} --event {native_event} --mode {}",
        client_value(client),
        mode_name(mode)
    )
}

const fn client_value(client: ClientId) -> &'static str {
    client.id()
}
