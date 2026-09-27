//! Claude Code signal bridge: native hook JSON -> normalized event.
//! Verified against the installed 2.1.283 hook schema (SessionStart
//! sources, SubagentStart, PreToolUse/PostToolUse `additionalContext`,
//! PreToolUse `permissionDecision`).

use serde_json::{Value, json};

use super::{
    ClientId, brainprint_action, has_unknown_keys, hook_command, is_all_files_glob, session_id,
    shell_action, str_field, unproven, usize_field,
};
use crate::event::{
    ActionClass, ClientCapabilities, EventKind, FallbackReason, IntegrationEvent, Mode,
    NativeAction, ResetSource, SearchPattern,
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
    // Verified on 2.1.283: a subagent's tool hooks carry `agent_id`/
    // `agent_type`; the main agent's carry neither.
    agent_scoped_tool_events: true,
};

pub const EVENTS: &[(&str, &str)] = &[
    ("SessionStart(startup)", "SESSION_STARTED"),
    ("SessionStart(resume|clear|compact)", "SESSION_RESET"),
    ("SubagentStart", "SESSION_STARTED(subagent)"),
    ("PreToolUse", "PRE_TOOL"),
    ("PostToolUse", "POST_TOOL"),
];

pub fn normalize(native_event: &str, payload: &Value) -> Result<IntegrationEvent, String> {
    let mut event = IntegrationEvent {
        kind: EventKind::PreTool,
        client_id: ClientId::ClaudeCode.id().to_owned(),
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
        other => return Err(format!("unsupported Claude Code event {other}")),
    }
    Ok(event)
}

fn action(tool: &str, input: &Value, response: Option<&Value>) -> NativeAction {
    if let Some(brainprint) = brainprint_action(tool, input, response) {
        return brainprint;
    }
    match tool {
        "Read" => read(input),
        "Grep" => grep(input),
        "Glob" => match (
            str_field(input, "pattern"),
            has_unknown_keys(input, &["pattern", "path"]),
        ) {
            (Some(pattern), false) if is_all_files_glob(pattern) => NativeAction::FileDiscovery {
                scope: str_field(input, "path").map(str::to_owned),
            },
            _ => unproven(
                ActionClass::ProjectTreeDiscovery,
                FallbackReason::UnsupportedFlags,
            ),
        },
        "Bash" => {
            if input.get("run_in_background").and_then(Value::as_bool) == Some(true) {
                NativeAction::Opaque
            } else {
                input
                    .get("command")
                    .map_or(NativeAction::Opaque, shell_action)
            }
        }
        "Edit" | "Write" | "MultiEdit" => {
            str_field(input, "file_path").map_or(NativeAction::Opaque, |path| NativeAction::Write {
                path: path.to_owned(),
            })
        }
        "NotebookEdit" => str_field(input, "notebook_path").map_or(NativeAction::Opaque, |path| {
            NativeAction::Write {
                path: path.to_owned(),
            }
        }),
        "TodoWrite" | "WebFetch" | "WebSearch" | "ToolSearch" | "AskUserQuestion" => {
            NativeAction::Inert
        }
        // Unknown tools (other MCP servers, subagent spawns, ...) may
        // change the project: never intercepted, but they invalidate.
        _ => NativeAction::Opaque,
    }
}

/// `Read{file_path, offset?, limit?}`. Whether `offset` counts from 0 or
/// 1 is not part of a stable contract, so the covered range is the union
/// of both readings -- a superset, which can only make coverage harder
/// to prove, never produce a false block.
fn read(input: &Value) -> NativeAction {
    let Some(path) = str_field(input, "file_path") else {
        return NativeAction::Opaque;
    };
    if has_unknown_keys(input, &["file_path", "offset", "limit"]) {
        return unproven(ActionClass::SourceRead, FallbackReason::UnsupportedFlags);
    }
    let lines = match (usize_field(input, "offset"), usize_field(input, "limit")) {
        (Some(offset), Some(limit)) => Some((offset.saturating_sub(1), offset + limit)),
        (None, Some(limit)) => Some((0, limit + 1)),
        // An open-ended read runs to an end-of-file this adapter never
        // learns (it does not read source).
        _ => None,
    };
    NativeAction::SourceRead {
        path: path.to_owned(),
        lines,
    }
}

/// `Grep` in `files_with_matches` (default) / `count` mode only.
fn grep(input: &Value) -> NativeAction {
    let Some(pattern) = str_field(input, "pattern") else {
        return NativeAction::Opaque;
    };
    let mode_ok = matches!(
        str_field(input, "output_mode"),
        None | Some("files_with_matches" | "count")
    );
    if !mode_ok {
        return unproven(ActionClass::TextSearch, FallbackReason::UnprovenEquivalence);
    }
    if has_unknown_keys(input, &["pattern", "path", "output_mode", "-i"]) {
        return unproven(ActionClass::TextSearch, FallbackReason::UnsupportedFlags);
    }
    NativeAction::TextSearch {
        pattern: SearchPattern::Regex(pattern.to_owned()),
        case_insensitive: input.get("-i").and_then(Value::as_bool).unwrap_or(false),
        scope: str_field(input, "path").map(str::to_owned),
    }
}

/// `.claude/settings.json` `hooks` fragment. Printed, never written.
pub fn config(mode: Mode, agent_command: &str) -> Value {
    let hook = |event: &str| json!([{ "type": "command", "command": hook_command(agent_command, ClientId::ClaudeCode, event, mode) }]);
    json!({
        "hooks": {
            "SessionStart": [{ "hooks": hook("SessionStart") }],
            "SubagentStart": [{ "hooks": hook("SubagentStart") }],
            "PreToolUse": [{ "matcher": "*", "hooks": hook("PreToolUse") }],
            "PostToolUse": [{ "matcher": "*", "hooks": hook("PostToolUse") }],
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_range_is_a_conservative_superset() {
        assert_eq!(
            read(&json!({"file_path": "/w/a.rs", "offset": 10, "limit": 5})),
            NativeAction::SourceRead {
                path: "/w/a.rs".into(),
                lines: Some((9, 15))
            }
        );
        assert_eq!(
            read(&json!({"file_path": "/w/a.rs"})),
            NativeAction::SourceRead {
                path: "/w/a.rs".into(),
                lines: None
            }
        );
        assert_eq!(
            read(&json!({"file_path": "/w/a.pdf", "pages": "1-2"})),
            unproven(ActionClass::SourceRead, FallbackReason::UnsupportedFlags)
        );
    }

    #[test]
    fn grep_content_mode_and_extra_flags_are_unproven() {
        assert_eq!(
            grep(&json!({"pattern": "x", "output_mode": "content"})),
            unproven(ActionClass::TextSearch, FallbackReason::UnprovenEquivalence)
        );
        assert_eq!(
            grep(&json!({"pattern": "x", "glob": "*.rs"})),
            unproven(ActionClass::TextSearch, FallbackReason::UnsupportedFlags)
        );
    }

    #[test]
    fn session_sources_map_to_start_or_reset() {
        let startup = normalize(
            "SessionStart",
            &json!({"session_id": "s", "source": "startup"}),
        )
        .unwrap();
        assert_eq!(startup.kind, EventKind::SessionStarted);
        let compact = normalize(
            "SessionStart",
            &json!({"session_id": "s", "source": "compact"}),
        )
        .unwrap();
        assert_eq!(
            (compact.kind, compact.reset_source),
            (EventKind::SessionReset, Some(ResetSource::Compaction))
        );
        assert!(
            normalize("PreToolUse", &json!({"tool_name": "Read"})).is_err(),
            "no session id"
        );
    }
}
