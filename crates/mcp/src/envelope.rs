//! MCP structured-result envelope (#25 "MCP result contract").
//!
//! The envelope's `result`/`error` payload is the actual Task 11
//! `QueryResultWire`/`QueryErrorWire`/`StatusResponse` value, serialized
//! as-is -- never a Rust `Debug` dump, never a second semantic
//! interpretation. `tool`/`mode`/`protocol_version`/`schema_version` are
//! wrapper metadata only; they never change what the payload means.
//!
//! One exception, always declared (#84): a text search's `binary_skipped`
//! path list longer than [`BINARY_SKIPPED_EXAMPLES`] is cut to its first
//! paths, and a `bounded_lists` entry names the field and its true
//! length. Every other field -- status, matches, the other scope lists
//! -- stays exact. The daemon wire (`brainprint find text --json`) keeps
//! the whole list.

use brainprint_core::PROTOCOL_VERSION;
use rmcp::model::CallToolResult;
use serde::Serialize;
use serde_json::{Map, Value, json};

/// The MCP wrapper's own schema version (envelope shape only -- not
/// Task 11's protocol version, which is carried separately). 2: the
/// optional `bounded_lists` key (#84).
const SCHEMA_VERSION: u32 = 2;

/// A text search's skipped-binary paths, inside the envelope.
const BINARY_SKIPPED: &str = "/payload/Find/Text/scope/binary_skipped";

/// How many of those paths the envelope keeps (#84). A skipped binary is
/// not an incompleteness, and the full list alone pushed whole-Workspace
/// results past Claude Code's inline limit -- which applies to
/// `structuredContent`, the part Claude Code shows the model.
const BINARY_SKIPPED_EXAMPLES: usize = 10;

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
/// derive `Serialize`, so nothing here re-shapes their fields, apart
/// from the declared `binary_skipped` bound.
pub fn build(tool: &str, mode: &str, outcome: Outcome, value: impl Serialize) -> CallToolResult {
    let mut payload = serde_json::to_value(value).unwrap_or(Value::Null);
    let bounded = bound_binary_skipped(&mut payload);
    let mut envelope = Map::new();
    envelope.insert("tool".to_owned(), json!(tool));
    envelope.insert("mode".to_owned(), json!(mode));
    envelope.insert("protocol_version".to_owned(), json!(PROTOCOL_VERSION));
    envelope.insert("schema_version".to_owned(), json!(SCHEMA_VERSION));
    envelope.insert(
        "outcome".to_owned(),
        json!(match outcome {
            Outcome::Ok => "ok",
            Outcome::BrainprintError => "brainprint_error",
            Outcome::TransportError => "transport_error",
        }),
    );
    // Before `payload`, so a reader meets it before the list it qualifies.
    if let Some(bounded) = bounded {
        envelope.insert("bounded_lists".to_owned(), json!([bounded]));
    }
    envelope.insert("payload".to_owned(), payload);
    let envelope = Value::Object(envelope);
    match outcome {
        Outcome::Ok => CallToolResult::structured(envelope),
        Outcome::BrainprintError | Outcome::TransportError => {
            CallToolResult::structured_error(envelope)
        }
    }
}

/// Cut a text search's `binary_skipped` to [`BINARY_SKIPPED_EXAMPLES`]
/// paths, returning the `bounded_lists` entry that says so. `None`, and
/// nothing touched, when the list is short or absent.
fn bound_binary_skipped(payload: &mut Value) -> Option<Value> {
    let list = payload
        .pointer_mut(&BINARY_SKIPPED["/payload".len()..])?
        .as_array_mut()?;
    let total = list.len();
    if total <= BINARY_SKIPPED_EXAMPLES {
        return None;
    }
    list.truncate(BINARY_SKIPPED_EXAMPLES);
    Some(json!({
        "field": BINARY_SKIPPED,
        "total": total,
        "shown": BINARY_SKIPPED_EXAMPLES,
        "note": "Examples only: the field lists the first `shown` of `total` skipped binary files, \
                 in walk order. A skipped binary is not an incompleteness; status and every other \
                 field are exact. The full list: `brainprint find text --json` with the same query, \
                 or a narrower path_prefix.",
    }))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn text_search(binary_skipped: usize) -> Value {
        json!({"Find": {"Text": {
            "status": "Unavailable",
            "matches": [],
            "scope": {
                "files_scanned": 3,
                "bytes_scanned": 30,
                "binary_skipped": (0..binary_skipped).map(|i| format!("bin/{i}.dll")).collect::<Vec<_>>(),
                "oversized_skipped": ["big.txt"],
                "unreadable": ["locked.txt"],
                "changed_during_scan": [],
                "budget_exhausted": null,
            },
            "structural_currentness": "Current",
            "reason": "ExplicitTextSearch",
        }}})
    }

    fn structured(result: &CallToolResult) -> Value {
        result
            .structured_content
            .clone()
            .expect("structured content")
    }

    #[test]
    fn a_long_binary_skip_list_is_bounded_and_declared_with_its_true_total() {
        let wire = text_search(467);
        let envelope = structured(&build("brainprint.find", "text", Outcome::Ok, &wire));

        assert_eq!(envelope["schema_version"], 2);
        assert_eq!(envelope["bounded_lists"][0]["field"], BINARY_SKIPPED);
        assert_eq!(envelope["bounded_lists"][0]["total"], 467);
        assert_eq!(
            envelope["bounded_lists"][0]["shown"],
            BINARY_SKIPPED_EXAMPLES
        );
        let shown = envelope.pointer(BINARY_SKIPPED).expect("list");
        assert_eq!(
            shown,
            &json!(
                (0..BINARY_SKIPPED_EXAMPLES)
                    .map(|i| format!("bin/{i}.dll"))
                    .collect::<Vec<_>>()
            ),
            "the first paths, in walk order"
        );

        let mut restored = envelope["payload"].clone();
        *restored
            .pointer_mut("/Find/Text/scope/binary_skipped")
            .expect("list") = wire
            .pointer("/Find/Text/scope/binary_skipped")
            .expect("list")
            .clone();
        assert_eq!(
            restored, wire,
            "only the binary list is cut: status, matches, oversized/unreadable/changed lists, \
             counts and currentness are exact"
        );
        assert!(
            serde_json::from_value::<brainprint_core::protocol::query::QueryResultWire>(
                envelope["payload"].clone()
            )
            .is_ok(),
            "the bounded payload still decodes as the wire type"
        );
    }

    #[test]
    fn a_short_binary_skip_list_leaves_the_payload_exact_and_undeclared() {
        let wire = text_search(BINARY_SKIPPED_EXAMPLES);
        let envelope = structured(&build("brainprint.find", "text", Outcome::Ok, &wire));

        assert_eq!(envelope["payload"], wire);
        assert!(envelope.get("bounded_lists").is_none());
    }
}
