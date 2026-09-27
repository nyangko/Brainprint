//! The normalized Integration Gateway vocabulary (#26 "Architecture",
//! #30 "Normalized event candidates").
//!
//! Every client bridge translates its native payload into exactly these
//! types; everything after normalization (`gateway`) is brand-free. A
//! field a client does not provide stays `None`/`false` -- never
//! estimated (#30 "client가 제공하지 않는 필드를 추정해서 채우지 않음").

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// #26 "Minimum normalized event vocabulary for Task 13", plus the
/// optional observed `CONTEXT_USAGE` (telemetry only, #26 acceptance 51).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventKind {
    SessionStarted,
    SessionReset,
    PreTool,
    PostTool,
    ContextUsage,
}

impl EventKind {
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "SESSION_STARTED" => Self::SessionStarted,
            "SESSION_RESET" => Self::SessionReset,
            "PRE_TOOL" => Self::PreTool,
            "POST_TOOL" => Self::PostTool,
            "CONTEXT_USAGE" => Self::ContextUsage,
            _ => return None,
        })
    }
}

/// Why a reset boundary happened (#29 "Reset boundary"), when observable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetSource {
    Startup,
    Resume,
    Clear,
    Compaction,
    Subagent,
}

impl ResetSource {
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "startup" => Self::Startup,
            "resume" => Self::Resume,
            "clear" => Self::Clear,
            "compaction" | "compact" => Self::Compaction,
            "subagent" => Self::Subagent,
            _ => return None,
        })
    }
}

/// #26 "Modes". `prefer` is the dogfood default; `guard` is opt-in only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Observe,
    Prefer,
    Guard,
}

/// #26 "Minimum capability flags", plus the two response channels the
/// common logic needs to place advice/bootstrap without branching on a
/// client brand (#26 acceptance 48).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientCapabilities {
    pub session_event: bool,
    pub reset_event: bool,
    pub pre_tool_event: bool,
    pub post_tool_event: bool,
    pub tool_selection_steering: bool,
    pub context_usage_signal: bool,
    pub notification_surface: bool,
    /// The pre-tool response can carry model-visible context without
    /// blocking.
    pub pre_tool_context: bool,
    /// The post-tool response can append model-visible context.
    pub post_tool_context: bool,
    /// The post-tool payload carries the tool's result, so a Brainprint
    /// delivery can be observed at all (#26 "Delivery observation").
    pub post_tool_result: bool,
    /// Tool events identify the (sub)agent issuing them, or the client
    /// has no subagents. Without it a parent's delivery could be taken
    /// as present in a subagent's context, so guard degrades to advice.
    pub agent_scoped_tool_events: bool,
}

/// One native tool invocation, already translated by a bridge into the
/// action classes #30 "Substitution Closure" names. The raw native
/// payload never travels past the bridge.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NativeAction {
    /// One of the four Task 12 Brainprint tools. `result` is the tool's
    /// result value, when the client exposes it (post-tool only).
    Brainprint {
        tool: BrainprintTool,
        #[serde(default)]
        input: Value,
        #[serde(default)]
        result: Option<Value>,
    },
    /// `SOURCE_READ` of one file. `lines` is a conservative 0-based,
    /// end-exclusive superset of the lines the native read may return;
    /// `None` means the whole file.
    SourceRead {
        path: String,
        #[serde(default)]
        lines: Option<(usize, usize)>,
    },
    /// `PROJECT_TREE_DISCOVERY`: every file under `scope`, recursively,
    /// with no name/type filter.
    FileDiscovery {
        #[serde(default)]
        scope: Option<String>,
    },
    /// `TEXT_SEARCH` whose output is derivable from a Brainprint text
    /// result's match list (which files / how many matches).
    TextSearch {
        pattern: SearchPattern,
        #[serde(default)]
        case_insensitive: bool,
        #[serde(default)]
        scope: Option<String>,
    },
    /// A recognized project-exploration action whose exact semantics
    /// could not be proven (unknown flag, complex shell, content-mode
    /// search, ...). Always allowed; counted for adoption telemetry.
    UnprovenExploration {
        class: ActionClass,
        reason: FallbackReason,
    },
    /// An edit/write whose target is known: invalidates that path's
    /// delivered-source descriptors. Never intercepted.
    Write { path: String },
    /// Anything else (arbitrary shell, build/test, git, network, ...):
    /// not project exploration, never intercepted (#26 "Native
    /// exploration classification"). Invalidates every descriptor,
    /// because its side effects are unknown.
    Opaque,
    /// A tool this gateway has nothing to say about and that cannot
    /// mutate the project (e.g. a todo list). Ignored entirely.
    Inert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrainprintTool {
    Find,
    Inspect,
    Relations,
    Context,
}

impl BrainprintTool {
    /// Recognize a Task 12 tool name through any client's MCP naming
    /// scheme (`brainprint.find`, `mcp__brainprint__brainprint_find`,
    /// `mcp_brainprint_brainprint.find`, ...): the Task 12 tool name is
    /// always the suffix, with `.` possibly rewritten to `_`.
    pub fn from_tool_name(name: &str) -> Option<Self> {
        let normalized = name.replace('.', "_");
        [
            ("brainprint_find", Self::Find),
            ("brainprint_inspect", Self::Inspect),
            ("brainprint_relations", Self::Relations),
            ("brainprint_context", Self::Context),
        ]
        .into_iter()
        .find(|(suffix, _)| normalized.ends_with(suffix))
        .map(|(_, tool)| tool)
    }
}

/// #30 canonical action classes, restricted to the ones Task 13 routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ActionClass {
    ProjectTreeDiscovery,
    TextSearch,
    SourceRead,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchPattern {
    Literal(String),
    Regex(String),
}

/// #26 "Fallback reasons" -- closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FallbackReason {
    NotInitialized,
    DaemonUnavailable,
    OutsideWorkspace,
    UnsupportedTool,
    UnsupportedFlags,
    Partial,
    StaleOrNotCurrent,
    Ambiguous,
    Truncated,
    UnprovenEquivalence,
    RequestExceedsDeliveredRange,
    UserBypass,
    BridgeError,
}

/// One normalized event (#26 "Normalized IntegrationEvent +
/// ClientCapabilities").
#[derive(Debug, Clone)]
pub struct IntegrationEvent {
    pub kind: EventKind,
    pub client_id: String,
    pub client_version: Option<String>,
    pub session_id: String,
    /// The session's working directory, when the client reports one.
    pub cwd: Option<String>,
    pub reset_source: Option<ResetSource>,
    /// The subagent a `SESSION_STARTED` starts (bootstrap once per id),
    /// or the subagent issuing a tool event. `None` = the main agent.
    pub agent_id: Option<String>,
    /// Whether this event's native response can carry model-visible
    /// context (bootstrap/advice). Decided by the bridge per event.
    pub can_inject_context: bool,
    pub action: Option<NativeAction>,
    pub capabilities: ClientCapabilities,
}

/// What the common logic decided. A bridge only formats this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Allow,
    Advise,
    SuppressRedirect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub kind: DecisionKind,
    /// Model-visible text: advice or the suppression/redirect reason.
    pub message: Option<String>,
    /// The thin bootstrap pointer, when this event is its (single)
    /// delivery point.
    pub bootstrap: Option<&'static str>,
    pub fallback: Option<FallbackReason>,
}

impl Decision {
    pub const fn allow() -> Self {
        Self {
            kind: DecisionKind::Allow,
            message: None,
            bootstrap: None,
            fallback: None,
        }
    }

    pub const fn fallback(reason: FallbackReason) -> Self {
        Self {
            kind: DecisionKind::Allow,
            message: None,
            bootstrap: None,
            fallback: Some(reason),
        }
    }

    /// Everything the model should see from this decision, bootstrap
    /// first. `None` when there is nothing to say.
    pub fn context_text(&self) -> Option<String> {
        match (self.bootstrap, self.message.as_deref()) {
            (None, None) => None,
            (Some(bootstrap), None) => Some(bootstrap.to_owned()),
            (None, Some(message)) => Some(message.to_owned()),
            (Some(bootstrap), Some(message)) => Some(format!("{bootstrap}\n\n{message}")),
        }
    }
}
