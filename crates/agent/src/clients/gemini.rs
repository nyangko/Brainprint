//! Gemini CLI signal bridge, against the installed 0.51.0 hook contract
//! (bundle/docs/hooks/reference.md): SessionStart(startup|resume|clear),
//! PreCompress (advisory only), BeforeTool (deny, no context), AfterTool
//! (`additionalContext`). BeforeToolSelection exists but P0 does not use
//! it: restricting the tool set is not an exact-safe suppression.

use serde_json::{Value, json};

use super::{
    ClientId, Rendered, brainprint_action, has_unknown_keys, hook_command, is_all_files_glob,
    session_id, shell_action, str_field, unproven, usize_field,
};
use crate::event::{
    ActionClass, ClientCapabilities, Decision, DecisionKind, EventKind, FallbackReason,
    IntegrationEvent, Mode, NativeAction, ResetSource,
};

pub const CAPABILITIES: ClientCapabilities = ClientCapabilities {
    session_event: true,
    reset_event: true,
    pre_tool_event: true,
    post_tool_event: true,
    tool_selection_steering: true,
    context_usage_signal: false,
    notification_surface: true,
    pre_tool_context: false,
    post_tool_context: true,
    post_tool_result: true,
    // Hook payloads carry no subagent identity: guard degrades to advice.
    agent_scoped_tool_events: false,
};

pub const EVENTS: &[(&str, &str)] = &[
    ("SessionStart(startup)", "SESSION_STARTED"),
    ("SessionStart(resume|clear)", "SESSION_RESET"),
    (
        "PreCompress",
        "SESSION_RESET(compaction, deferred bootstrap)",
    ),
    ("BeforeTool", "PRE_TOOL"),
    ("AfterTool", "POST_TOOL"),
];

pub fn normalize(native_event: &str, payload: &Value) -> Result<IntegrationEvent, String> {
    let mut event = IntegrationEvent {
        kind: EventKind::PreTool,
        client_id: ClientId::GeminiCli.id().to_owned(),
        client_version: None,
        session_id: session_id(payload)?,
        cwd: str_field(payload, "cwd").map(str::to_owned),
        reset_source: None,
        agent_id: None,
        can_inject_context: false,
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
            event.can_inject_context = true;
        }
        "PreCompress" => {
            event.kind = EventKind::SessionReset;
            event.reset_source = Some(ResetSource::Compaction);
        }
        "BeforeTool" | "AfterTool" => {
            let is_after = native_event == "AfterTool";
            event.kind = if is_after {
                EventKind::PostTool
            } else {
                EventKind::PreTool
            };
            event.can_inject_context = is_after;
            let tool = str_field(payload, "tool_name").ok_or("tool event without tool_name")?;
            let input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
            event.action = Some(action(tool, &input, payload.get("tool_response")));
        }
        other => return Err(format!("unsupported Gemini CLI event {other}")),
    }
    Ok(event)
}

fn action(tool: &str, input: &Value, response: Option<&Value>) -> NativeAction {
    if let Some(brainprint) = brainprint_action(tool, input, response) {
        return brainprint;
    }
    match tool {
        // `offset` is documented 0-based.
        "read_file" => {
            let Some(path) =
                str_field(input, "file_path").or_else(|| str_field(input, "absolute_path"))
            else {
                return NativeAction::Opaque;
            };
            if has_unknown_keys(input, &["file_path", "absolute_path", "offset", "limit"]) {
                return unproven(ActionClass::SourceRead, FallbackReason::UnsupportedFlags);
            }
            let lines = match (usize_field(input, "offset"), usize_field(input, "limit")) {
                (offset, Some(limit)) => {
                    let offset = offset.unwrap_or(0);
                    Some((offset, offset + limit))
                }
                _ => None,
            };
            NativeAction::SourceRead {
                path: path.to_owned(),
                lines,
            }
        }
        "read_many_files" => unproven(ActionClass::SourceRead, FallbackReason::UnsupportedTool),
        "glob" => {
            let git_ignore_default = input
                .get("respect_git_ignore")
                .is_none_or(|value| value.as_bool() == Some(true));
            match str_field(input, "pattern") {
                Some(pattern)
                    if is_all_files_glob(pattern)
                        && git_ignore_default
                        && !has_unknown_keys(
                            input,
                            &["pattern", "path", "case_sensitive", "respect_git_ignore"],
                        ) =>
                {
                    NativeAction::FileDiscovery {
                        scope: str_field(input, "path").map(str::to_owned),
                    }
                }
                _ => unproven(
                    ActionClass::ProjectTreeDiscovery,
                    FallbackReason::UnsupportedFlags,
                ),
            }
        }
        // Returns matching lines (content mode): not derivable from a
        // Brainprint match list verbatim.
        "grep_search" | "search_file_content" => {
            unproven(ActionClass::TextSearch, FallbackReason::UnprovenEquivalence)
        }
        "list_directory" => unproven(
            ActionClass::ProjectTreeDiscovery,
            FallbackReason::UnprovenEquivalence,
        ),
        "run_shell_command" => input
            .get("command")
            .map_or(NativeAction::Opaque, shell_action),
        "write_file" | "replace" => {
            str_field(input, "file_path").map_or(NativeAction::Opaque, |path| NativeAction::Write {
                path: path.to_owned(),
            })
        }
        "web_fetch" | "google_web_search" | "write_todos" | "save_memory" => NativeAction::Inert,
        _ => NativeAction::Opaque,
    }
}

/// Gemini parses stdout as JSON on exit 0, so a no-op is `{}`.
pub fn render(native_event: &str, decision: &Decision) -> Rendered {
    let body = match native_event {
        "BeforeTool" if decision.kind == DecisionKind::SuppressRedirect => json!({
            "decision": "deny",
            "reason": decision.message.clone().unwrap_or_default(),
        }),
        "SessionStart" | "AfterTool" => match decision.context_text() {
            Some(context) => json!({
                "hookSpecificOutput": {
                    "hookEventName": native_event,
                    "additionalContext": context,
                }
            }),
            None => json!({}),
        },
        _ => json!({}),
    };
    Rendered {
        stdout: body.to_string(),
        exit_code: 0,
    }
}

/// `.gemini/settings.json` fragment. Printed, never written.
pub fn config(mode: Mode, agent_command: &str) -> Value {
    let hook = |event: &str| json!([{ "type": "command", "command": hook_command(agent_command, ClientId::GeminiCli, event, mode), "name": format!("brainprint-{}", event.to_lowercase()) }]);
    json!({
        "hooksConfig": { "enabled": true },
        "hooks": {
            "SessionStart": [{ "hooks": hook("SessionStart") }],
            "PreCompress": [{ "hooks": hook("PreCompress") }],
            "BeforeTool": [{ "matcher": ".*", "hooks": hook("BeforeTool") }],
            "AfterTool": [{ "matcher": ".*", "hooks": hook("AfterTool") }],
        }
    })
}
