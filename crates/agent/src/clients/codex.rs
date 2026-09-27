//! Codex CLI signal bridge, against the installed 0.157.1 hooks feature
//! (stable): `hooks.json` with SessionStart / SubagentStart / PreToolUse
//! / PostToolUse / PostCompact, using the same response wire as Claude
//! Code (`hookSpecificOutput.permissionDecision|additionalContext`).
//! Codex explores through shell commands, so its native reads/searches
//! reach the gateway through the common conservative shell classifier.

use serde_json::{Value, json};

use super::{ClientId, brainprint_action, hook_command, session_id, shell_action, str_field};
use crate::event::{
    ClientCapabilities, EventKind, IntegrationEvent, Mode, NativeAction, ResetSource,
};

pub const CAPABILITIES: ClientCapabilities = ClientCapabilities {
    session_event: true,
    reset_event: true,
    pre_tool_event: true,
    post_tool_event: true,
    tool_selection_steering: false,
    context_usage_signal: false,
    notification_surface: true,
    pre_tool_context: true,
    post_tool_context: true,
    post_tool_result: true,
    // Verified on 0.157.1: a subagent's tool hooks carry `agent_id`/
    // `agent_type`; the main agent's carry neither.
    agent_scoped_tool_events: true,
};

pub const EVENTS: &[(&str, &str)] = &[
    ("SessionStart(startup)", "SESSION_STARTED"),
    ("SessionStart(resume|clear|compact)", "SESSION_RESET"),
    (
        "PostCompact",
        "SESSION_RESET(compaction, deferred bootstrap)",
    ),
    ("SubagentStart", "SESSION_STARTED(subagent)"),
    ("PreToolUse", "PRE_TOOL"),
    ("PostToolUse", "POST_TOOL"),
];

pub fn normalize(native_event: &str, payload: &Value) -> Result<IntegrationEvent, String> {
    let mut event = IntegrationEvent {
        kind: EventKind::PreTool,
        client_id: ClientId::CodexCli.id().to_owned(),
        client_version: None,
        session_id: session_id(payload)?,
        cwd: str_field(payload, "cwd").map(str::to_owned),
        reset_source: None,
        agent_id: str_field(payload, "agent_id").map(str::to_owned),
        can_inject_context: true,
        action: None,
        capabilities: CAPABILITIES,
    };
    match native_event {
        "SessionStart" => {
            let source = str_field(payload, "source").unwrap_or("startup");
            let source = ResetSource::parse(source)
                .ok_or_else(|| format!("unknown SessionStart source {source}"))?;
            event.kind = if source == ResetSource::Startup {
                EventKind::SessionStarted
            } else {
                EventKind::SessionReset
            };
            event.reset_source = Some(source);
            event.agent_id = None;
        }
        "PostCompact" => {
            event.kind = EventKind::SessionReset;
            event.reset_source = Some(ResetSource::Compaction);
            event.can_inject_context = false;
            event.agent_id = None;
        }
        "SubagentStart" => {
            event.kind = EventKind::SessionStarted;
            event.reset_source = Some(ResetSource::Subagent);
            event.agent_id = Some(
                str_field(payload, "agent_id")
                    .ok_or("SubagentStart without agent_id")?
                    .to_owned(),
            );
        }
        "PreToolUse" | "PostToolUse" => {
            event.kind = if native_event == "PreToolUse" {
                EventKind::PreTool
            } else {
                EventKind::PostTool
            };
            let tool = str_field(payload, "tool_name").ok_or("tool event without tool_name")?;
            let input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
            event.action = Some(action(tool, &input, payload.get("tool_response")));
        }
        other => return Err(format!("unsupported Codex CLI event {other}")),
    }
    Ok(event)
}

fn action(tool: &str, input: &Value, response: Option<&Value>) -> NativeAction {
    if let Some(brainprint) = brainprint_action(tool, input, response) {
        return brainprint;
    }
    match tool {
        "Bash" | "shell" | "shell_command" | "exec_command" | "local_shell" | "unified_exec" => {
            input
                .get("command")
                .or_else(|| input.get("cmd"))
                .map_or(NativeAction::Opaque, shell_action)
        }
        "update_plan" | "view_image" | "web_search" => NativeAction::Inert,
        // apply_patch and anything unknown: may write, paths not modelled.
        _ => NativeAction::Opaque,
    }
}

/// `hooks.json` fragment. Printed, never written.
pub fn config(mode: Mode, agent_command: &str) -> Value {
    let hook = |event: &str| json!([{ "type": "command", "command": hook_command(agent_command, ClientId::CodexCli, event, mode) }]);
    json!({
        "hooks": {
            "SessionStart": [{ "hooks": hook("SessionStart") }],
            "SubagentStart": [{ "hooks": hook("SubagentStart") }],
            "PostCompact": [{ "hooks": hook("PostCompact") }],
            "PreToolUse": [{ "matcher": "*", "hooks": hook("PreToolUse") }],
            "PostToolUse": [{ "matcher": "*", "hooks": hook("PostToolUse") }],
        }
    })
}
