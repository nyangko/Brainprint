//! MCP structured-result envelope (#25 "MCP result contract").
//!
//! The envelope's `result`/`error` payload is the actual Task 11
//! `QueryResultWire`/`QueryErrorWire`/`StatusResponse` value, serialized
//! as-is -- never a Rust `Debug` dump, never a second semantic
//! interpretation. `tool`/`mode`/`protocol_version`/`schema_version` are
//! wrapper metadata only; they never change what the payload means.

use brainprint_core::PROTOCOL_VERSION;
use rmcp::model::CallToolResult;
use serde::Serialize;
use serde_json::json;

/// The MCP wrapper's own schema version (envelope shape only -- not
/// Task 11's protocol version, which is carried separately).
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A valid Task 10/11 answer (`QueryOutcomeWire::Ok`, or the
    /// daemon's `Status` response for `context status`). Ordinary Core
    /// answer states -- not-found, ambiguous, not-current, partial
    /// coverage, truncation -- are `Ok` here too (#24), never `Error`.
    Ok,
    /// A valid typed Brainprint error (#25 "Error mapping" layer 3):
    /// `QueryOutcomeWire::Err`. Still fully structured and inspectable,
    /// never collapsed into prose.
    BrainprintError,
    /// #25 "Error mapping" layer 2: daemon unavailable, protocol
    /// mismatch, local IPC failure.
    TransportError,
}

/// Build the structured `CallToolResult` for one tool call.
///
/// `value` is serialized exactly as returned by Task 11/the daemon --
/// `QueryResultWire`, `QueryErrorWire`, or `StatusResponse` all already
/// derive `Serialize`, so nothing here re-shapes their fields.
pub fn build(tool: &str, mode: &str, outcome: Outcome, value: impl Serialize) -> CallToolResult {
    let payload = json!({
        "tool": tool,
        "mode": mode,
        "protocol_version": PROTOCOL_VERSION,
        "schema_version": SCHEMA_VERSION,
        "outcome": match outcome {
            Outcome::Ok => "ok",
            Outcome::BrainprintError => "brainprint_error",
            Outcome::TransportError => "transport_error",
        },
        "payload": value,
    });
    match outcome {
        Outcome::Ok => CallToolResult::structured(payload),
        Outcome::BrainprintError | Outcome::TransportError => {
            CallToolResult::structured_error(payload)
        }
    }
}

/// A layer-2 transport failure (#25 "Error mapping"): daemon unavailable,
/// protocol mismatch, local IPC failure. Compact and actionable per #25
/// "Transport", never a raw driver/debug dump -- `DaemonError`'s
/// `Display` already guarantees that.
pub fn transport_error(tool: &str, mode: &str, message: impl std::fmt::Display) -> CallToolResult {
    build(
        tool,
        mode,
        Outcome::TransportError,
        json!({ "message": message.to_string() }),
    )
}

/// A layer-1 malformed-request problem this adapter caught before
/// sending anything to the daemon (#25 "Error mapping"): the caller's
/// job to fix, not the daemon's. A protocol `invalid_params` error, not
/// a tool-level result -- the caller could not have gotten a Brainprint
/// answer for a request Brainprint never saw.
pub fn invalid_params(message: impl Into<String>) -> rmcp::ErrorData {
    rmcp::ErrorData::invalid_params(message.into(), None)
}
