//! The Svelte language server's wire surface, as typed requests and
//! answers.
//!
//! #19 task 11 opened with an executable spike rather than with the
//! design document, because the TypeScript ecosystem moved after #19
//! was written. What the spike measured against the pinned install:
//!
//! ```text
//! package        svelte-language-server@0.18.4
//! launch         node <root>/node_modules/svelte-language-server/bin/server.js --stdio
//! serverInfo     absent
//! positionEncoding  absent -> LSP's default, UTF-16
//! textDocumentSync  { openClose: true, change: 2 }  (available, not needed)
//! ```
//!
//! Three measurements decided this module's shape.
//!
//! **Answers already arrive in original `.svelte` coordinates.** Asked
//! for the definition of `{increment}` in a component's markup, the
//! server answers `src/Parent.svelte:8:13`, which is where `increment`
//! is declared in the *component*, not in the `svelte2tsx` output it
//! generated to type-check. Every case in the fixture behaved this way,
//! so Brainprint consumes that boundary instead of reconstructing a
//! source map -- and [`is_generated_uri`] is the guard that keeps it
//! honest rather than assumed.
//!
//! **An unsynchronized document is an error, never an empty answer.**
//! Asking about a `.svelte` file the server has not been told about
//! returns `Cannot call methods on an unopened document`. That is the
//! ideal failure mode: a missing synchronization cannot arrive as "no
//! findings", so a false zero is structurally impossible here.
//!
//! **The server executes project configuration.** `svelte.config.js` is
//! loaded through `@sveltejs/load-config` at startup -- measured, with a
//! config that wrote a marker file. See [`IS_TRUSTED`].

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::lsp::coordinates::{Position, PositionEncoding, Range};

// ---------------------------------------------------------------------
// Backend identity and compatibility
// ---------------------------------------------------------------------

/// The `svelte-language-server` version #19 task 11 measured.
pub const TESTED_SERVER_VERSION: &str = "0.18.4";

/// The `svelte2tsx` that produced every mapping the spike observed.
pub const TESTED_SVELTE2TSX_VERSION: &str = "0.7.61";

/// The TypeScript the Svelte backend runs its language service on.
///
/// **Not** the 7.0.2 the TS/JS backend uses, and that is a measurement
/// rather than an oversight. `svelte-language-server@0.18.4` declares
/// `peerDependencies.typescript` as `^5.9.2 || ^6.0.2`, so npm refuses
/// to install it beside `typescript@7.0.2`; forced through with
/// `--legacy-peer-deps`, the server dies on module load reading
/// `ts.sys.useCaseSensitiveFileNames` off an `undefined`, because
/// TypeScript 7 is the native port and no longer exposes that CommonJS
/// shape.
///
/// So Brainprint runs two semantic backends on two TypeScript versions.
/// [`SemanticBackendKind::Svelte`](crate::semantic::SemanticBackendKind)
/// exists for exactly this, and forcing one shared process for symmetry
/// would mean no Svelte semantics at all.
pub const TESTED_TYPESCRIPT_VERSION: &str = "6.0.3";

/// The compatibility class recorded in
/// [`ToolchainIdentity::backend_compatibility_class`](crate::semantic::ToolchainIdentity).
///
/// Major.minor. The language server is pre-1.0 and its patch line moves
/// often; a `0.19` may move a response shape, and reading it as `0.18`
/// would turn a protocol change into wrong semantic facts rather than
/// into an honest gap.
pub const COMPATIBILITY_CLASS: &str = "svelte-language-server:0.18";

/// Whether a located install is one this adapter may parse answers from.
///
/// Classified from the **package manifest**, not from the handshake.
/// That is forced: the measured server reports no `serverInfo` at all,
/// so there is nothing in the protocol to identify it with. The manifest
/// is read before launch, which is also what [`AnalysisContext`](crate::semantic::AnalysisContext)
/// needs -- the context key selects the runtime, so it cannot depend on
/// a running one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolCompatibility {
    /// Exactly [`TESTED_SERVER_VERSION`].
    Tested,
    /// Another patch inside [`COMPATIBILITY_CLASS`].
    CompatiblePatch,
    /// Anything else. Nothing is parsed.
    Unknown,
}

impl ProtocolCompatibility {
    #[must_use]
    pub fn classify(version: &str) -> Self {
        if version == TESTED_SERVER_VERSION {
            return Self::Tested;
        }
        let tested: Vec<&str> = TESTED_SERVER_VERSION.split('.').collect();
        let found: Vec<&str> = version.split('.').collect();
        if found.len() == 3
            && found.iter().all(|part| {
                !part.is_empty() && part.chars().all(|character| character.is_ascii_digit())
            })
            && found[0] == tested[0]
            && found[1] == tested[1]
        {
            return Self::CompatiblePatch;
        }
        Self::Unknown
    }

    #[must_use]
    pub const fn usable(self) -> bool {
        matches!(self, Self::Tested | Self::CompatiblePatch)
    }

    /// What this verdict means for a publication already stored.
    #[must_use]
    pub const fn publication_verdict(self) -> crate::semantic_index::BackendCompatibility {
        use crate::semantic_index::BackendCompatibility;
        match self {
            Self::Tested | Self::CompatiblePatch => BackendCompatibility::Compatible,
            Self::Unknown => BackendCompatibility::Unknown,
        }
    }
}

impl fmt::Display for ProtocolCompatibility {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Tested => "TESTED",
            Self::CompatiblePatch => "COMPATIBLE_PATCH",
            Self::Unknown => "UNKNOWN",
        })
    }
}

// ---------------------------------------------------------------------
// Trust
// ---------------------------------------------------------------------

/// What Brainprint tells the server about trusting the project.
///
/// `false`, always, and this is the single most important constant in
/// the module. The spike measured a plain `svelte.config.js` running
/// arbitrary project code during initialization -- it wrote a file --
/// and Brainprint's locked model does not automatically trust project
/// plugin or config code just because it runs in a child process.
///
/// The switch is the server's own, not an internal reached into:
/// `server.js` reads `initializationOptions.isTrusted` (defaulting to
/// `true`) and calls `configLoader.setDisabled(!isTrusted)`, which also
/// stops it loading project packages such as preprocessors.
///
/// It costs nothing this tier claims. Every P0 capability was measured
/// answering identically with the config present, absent, broken, and
/// blocked: template-to-script, component-to-component and script type
/// resolution were byte-identical in all four runs. A project whose
/// semantics genuinely need a preprocessor degrades with an explicit
/// limitation, which is the stated trade.
pub const IS_TRUSTED: bool = false;

/// The position encoding this connection counts in.
///
/// UTF-16, because the measured server reports no `positionEncoding` and
/// LSP says that means UTF-16. Deliberately *not* negotiated down to
/// UTF-8 the way the TypeScript 7 backend is: this server did not offer
/// it, and assuming it would put every span at a plausible wrong offset.
pub const POSITION_ENCODING: PositionEncoding = PositionEncoding::Utf16;

// ---------------------------------------------------------------------
// Wire constants
// ---------------------------------------------------------------------

/// JSON-RPC `MethodNotFound`.
pub const METHOD_NOT_FOUND: i64 = -32601;

/// JSON-RPC `InternalError`. What the measured server answers for a
/// document it has not been told about -- an error, which is the point.
pub const INTERNAL_ERROR: i64 = -32603;

/// The message the server uses for a document it has not synchronized.
///
/// Recognised so the adapter can report "this component was never
/// synchronized" as itself rather than as a backend fault. It can only
/// happen through a bug in this tier, because synchronization is what
/// [`crate::svelte_semantic::lifecycle::synchronize`] guarantees before
/// any question is asked.
pub const UNOPENED_DOCUMENT: &str = "Cannot call methods on an unopened document";

pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const INITIALIZED: &str = "initialized";
    pub const SHUTDOWN: &str = "shutdown";
    pub const EXIT: &str = "exit";
    pub const CONFIGURATION: &str = "workspace/configuration";
    pub const REGISTER_CAPABILITY: &str = "client/registerCapability";
    pub const UNREGISTER_CAPABILITY: &str = "client/unregisterCapability";
    pub const DID_CHANGE_WATCHED_FILES: &str = "workspace/didChangeWatchedFiles";
    pub const DEFINITION: &str = "textDocument/definition";
    /// Never sent. Named so the "filesystem truth, no editor overlay"
    /// decision is checkable rather than only documented.
    pub const DID_OPEN: &str = "textDocument/didOpen";
    /// Never sent. See [`DID_OPEN`].
    pub const DID_CHANGE: &str = "textDocument/didChange";
    /// Never sent. See [`DID_OPEN`].
    pub const DID_CLOSE: &str = "textDocument/didClose";
}

/// The document-synchronization notifications this adapter must never
/// emit.
///
/// Task 11 asked whether `.svelte` needs an overlay, since unlike
/// TypeScript the server must generate an intermediate representation
/// before it can answer. Measured: it does not. From a cold start, with
/// a single `workspace/didChangeWatchedFiles` and no settle delay, five
/// consecutive definition requests answered correctly every time, and
/// the same held across an edit, a file creation and a deletion. The
/// overlay was measured too and was identical -- so it buys nothing and
/// costs the one thing that matters, which is that an editor buffer
/// would become a second reality beside the Workspace filesystem
/// Brainprint indexed.
pub const FORBIDDEN_SYNC_METHODS: [&str; 3] =
    [method::DID_OPEN, method::DID_CHANGE, method::DID_CLOSE];

// ---------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------

/// What kind of filesystem change a watched-file notification reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WatchedChangeKind {
    Created,
    Changed,
    Deleted,
}

impl WatchedChangeKind {
    const fn wire(self) -> i64 {
        match self {
            Self::Created => 1,
            Self::Changed => 2,
            Self::Deleted => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchedChange {
    pub uri: String,
    pub kind: WatchedChangeKind,
}

/// One question for the Svelte backend.
///
/// A deliberately tiny surface. Task 11 needs one semantic method, and
/// wrapping the server's other twenty providers would be twenty
/// capability claims with no code behind them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SvelteRequest {
    Definition { uri: String, position: Position },
    WatchedFilesChanged { changes: Vec<WatchedChange> },
}

impl SvelteRequest {
    #[must_use]
    pub fn wire(&self) -> (&'static str, Option<Value>) {
        match self {
            Self::Definition { uri, position } => (
                method::DEFINITION,
                Some(json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": position.line, "character": position.character },
                })),
            ),
            Self::WatchedFilesChanged { changes } => (
                method::DID_CHANGE_WATCHED_FILES,
                Some(json!({
                    "changes": changes
                        .iter()
                        .map(|change| json!({ "uri": change.uri, "type": change.kind.wire() }))
                        .collect::<Vec<Value>>(),
                })),
            ),
        }
    }

    /// Whether this is a notification rather than a request.
    ///
    /// Load-bearing: a notification has no reply to wait for, and
    /// JSON-RPC over one ordered stdio connection delivers it strictly
    /// before anything sent after it. That ordering is this backend's
    /// synchronization barrier.
    #[must_use]
    pub const fn is_notification(&self) -> bool {
        matches!(self, Self::WatchedFilesChanged { .. })
    }

    #[must_use]
    pub fn uri(&self) -> Option<&str> {
        match self {
            Self::Definition { uri, .. } => Some(uri),
            Self::WatchedFilesChanged { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------
// File URIs
// ---------------------------------------------------------------------

/// A `file:` URI for an absolute path.
#[must_use]
pub fn path_to_uri(path: &std::path::Path) -> String {
    crate::python_semantic::protocol::path_to_uri(path)
}

/// The absolute path a `file:` URI names.
#[must_use]
pub fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    crate::python_semantic::protocol::uri_to_path(uri)
}

/// Whether a URI names `svelte2tsx` output rather than real source.
///
/// The measured server never returned one -- every answer was already in
/// original `.svelte` coordinates. This exists so that stays a *checked*
/// fact rather than a remembered one: a future version that leaked a
/// generated position would produce an explicit gap here instead of a
/// span pointing into a file no human wrote.
///
/// The markers are the ones `svelte2tsx` actually emits: the synthetic
/// `__sveltets_*` helpers, and the `.svelte.ts` / `.svelte.tsx` shadow
/// files it names its output after.
#[must_use]
pub fn is_generated_uri(uri: &str) -> bool {
    uri.contains("__sveltets")
        || uri.ends_with(".svelte.ts")
        || uri.ends_with(".svelte.tsx")
        || uri.ends_with(".svelte.js")
        || uri.ends_with(".svelte.jsx")
}

// ---------------------------------------------------------------------
// Answers
// ---------------------------------------------------------------------

/// One place in one document, as the backend names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

/// What the backend answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SvelteResponse {
    Locations(Vec<Location>),
    /// The backend does not implement the method. Recorded rather than
    /// turned into an empty result.
    Unsupported(String),
    /// The backend was asked about a document nobody synchronized.
    /// A bug in this tier, and it reads as one rather than as a fact
    /// about the component.
    Unsynchronized(String),
    /// A notification was delivered.
    Delivered,
}

/// Why a backend answer could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError {
    pub method: &'static str,
    pub detail: String,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.method, self.detail)
    }
}

impl std::error::Error for DecodeError {}

fn bad(method: &'static str, detail: impl Into<String>) -> DecodeError {
    DecodeError {
        method,
        detail: detail.into(),
    }
}

fn position(value: &Value, method: &'static str) -> Result<Position, DecodeError> {
    let line = value
        .get("line")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad(method, "position without a line"))?;
    let character = value
        .get("character")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad(method, "position without a character"))?;
    Ok(Position::new(
        u32::try_from(line).map_err(|_| bad(method, "line out of range"))?,
        u32::try_from(character).map_err(|_| bad(method, "character out of range"))?,
    ))
}

fn range(value: &Value, method: &'static str) -> Result<Range, DecodeError> {
    let start = value
        .get("start")
        .ok_or_else(|| bad(method, "range without a start"))?;
    let end = value
        .get("end")
        .ok_or_else(|| bad(method, "range without an end"))?;
    Ok(Range::new(position(start, method)?, position(end, method)?))
}

/// One `Location`, one `LocationLink`, or neither.
fn location(value: &Value, method: &'static str) -> Result<Location, DecodeError> {
    if let (Some(uri), Some(raw)) = (value.get("uri").and_then(Value::as_str), value.get("range")) {
        return Ok(Location {
            uri: uri.to_owned(),
            range: range(raw, method)?,
        });
    }
    if let Some(uri) = value.get("targetUri").and_then(Value::as_str) {
        let raw = value
            .get("targetSelectionRange")
            .or_else(|| value.get("targetRange"))
            .ok_or_else(|| bad(method, "location link without a target range"))?;
        return Ok(Location {
            uri: uri.to_owned(),
            range: range(raw, method)?,
        });
    }
    Err(bad(method, "neither a Location nor a LocationLink"))
}

/// Read what the server answered into the typed shape the request asked
/// for.
///
/// Every unexpected shape is a [`DecodeError`], never an empty success.
///
/// # Errors
/// When the answer is a shape this build does not read.
pub fn decode(request: &SvelteRequest, result: &Value) -> Result<SvelteResponse, DecodeError> {
    let (name, _) = request.wire();
    match request {
        SvelteRequest::Definition { .. } => Ok(SvelteResponse::Locations(match result {
            Value::Null => Vec::new(),
            Value::Array(items) => items
                .iter()
                .map(|item| location(item, name))
                .collect::<Result<Vec<_>, _>>()?,
            single => vec![location(single, name)?],
        })),
        SvelteRequest::WatchedFilesChanged { .. } => Ok(SvelteResponse::Delivered),
    }
}

/// The `initialize` parameters this backend is driven with.
///
/// Read it as decisions rather than boilerplate:
///
/// * `initializationOptions.isTrusted` is `false`. See [`IS_TRUSTED`].
/// * `workspace.didChangeWatchedFiles.dynamicRegistration` is `true`,
///   which is what makes the server take its file watching from the
///   client and gives the ordering barrier.
/// * `workspace.configuration` is `true` because the server asks, and an
///   unanswered `workspace/configuration` wedges the connection.
/// * `textDocument.synchronization` is absent, deliberately. This client
///   never opens a document.
#[must_use]
pub fn initialize_params(root_uri: &str, process_id: u32) -> Value {
    json!({
        "processId": process_id,
        "rootUri": root_uri,
        "initializationOptions": {
            // The whole trust boundary, in one field.
            "isTrusted": IS_TRUSTED,
            "dontFilterIncompleteCompletions": true,
        },
        "capabilities": {
            "general": { "positionEncodings": ["utf-16"] },
            "textDocument": {
                "definition": { "linkSupport": true },
            },
            "workspace": {
                "workspaceFolders": true,
                "configuration": true,
                "didChangeWatchedFiles": { "dynamicRegistration": true },
            },
        },
        "workspaceFolders": [{ "uri": root_uri, "name": "workspace" }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic_index::BackendCompatibility;

    #[test]
    fn the_measured_server_is_the_tested_one() {
        assert_eq!(
            ProtocolCompatibility::classify(TESTED_SERVER_VERSION),
            ProtocolCompatibility::Tested
        );
        assert!(ProtocolCompatibility::Tested.usable());
    }

    #[test]
    fn a_patch_inside_the_class_stays_comparable_and_a_minor_does_not() {
        assert_eq!(
            ProtocolCompatibility::classify("0.18.9"),
            ProtocolCompatibility::CompatiblePatch
        );
        for version in ["0.19.0", "1.0.0", "0.17.9", "", "next"] {
            assert_eq!(
                ProtocolCompatibility::classify(version),
                ProtocolCompatibility::Unknown,
                "{version} must not be read as 0.18"
            );
        }
        let verdict = ProtocolCompatibility::Unknown.publication_verdict();
        assert_eq!(verdict, BackendCompatibility::Unknown);
        assert!(!verdict.keeps_publication());
    }

    #[test]
    fn the_handshake_declares_the_project_untrusted() {
        // The measured server executes `svelte.config.js` unless told
        // otherwise, and its default is trusted. This is the one field
        // that stops project code running.
        let params = initialize_params("file:///w", 1);
        assert_eq!(params["initializationOptions"]["isTrusted"], false);
        assert_eq!(params["initializationOptions"]["isTrusted"], IS_TRUSTED);
    }

    #[test]
    fn the_handshake_never_offers_document_synchronization() {
        let params = initialize_params("file:///w", 1);
        assert!(
            params["capabilities"]["textDocument"]
                .get("synchronization")
                .is_none()
        );
        assert_eq!(
            params["capabilities"]["workspace"]["didChangeWatchedFiles"]["dynamicRegistration"],
            true,
            "client-side watching is what gives an ordering barrier"
        );
        assert_eq!(params["capabilities"]["workspace"]["configuration"], true);
    }

    #[test]
    fn no_document_sync_method_is_a_request_this_type_can_express() {
        // The strongest form the decision can take: there is no variant
        // that emits one, so it is not a policy that can be forgotten.
        let emitted: Vec<&str> = [
            SvelteRequest::Definition {
                uri: "file:///a.svelte".into(),
                position: Position::new(0, 0),
            },
            SvelteRequest::WatchedFilesChanged {
                changes: Vec::new(),
            },
        ]
        .iter()
        .map(|request| request.wire().0)
        .collect();
        for forbidden in FORBIDDEN_SYNC_METHODS {
            assert!(!emitted.contains(&forbidden), "{forbidden}");
        }
    }

    #[test]
    fn a_watched_change_carries_the_wire_numbers_lsp_defines() {
        let request = SvelteRequest::WatchedFilesChanged {
            changes: vec![
                WatchedChange {
                    uri: "file:///new.svelte".into(),
                    kind: WatchedChangeKind::Created,
                },
                WatchedChange {
                    uri: "file:///old.svelte".into(),
                    kind: WatchedChangeKind::Deleted,
                },
            ],
        };
        let (name, params) = request.wire();
        assert_eq!(name, method::DID_CHANGE_WATCHED_FILES);
        assert!(request.is_notification());
        let changes = params.expect("params");
        assert_eq!(changes["changes"][0]["type"], 1);
        assert_eq!(changes["changes"][1]["type"], 3);
    }

    #[test]
    fn the_measured_component_answer_decodes_to_the_file_it_names() {
        // Verbatim from the spike: a component tag answers with the
        // `.svelte` file at a degenerate position, which is the server
        // saying "this component", not "this declaration".
        let request = SvelteRequest::Definition {
            uri: "file:///w/src/Parent.svelte".into(),
            position: Position::new(16, 5),
        };
        let measured = json!([{
            "uri": "file:///w/src/lib/Child.svelte",
            "range": { "start": { "line": 0, "character": 1 },
                       "end": { "line": 0, "character": 1 } },
        }]);
        let SvelteResponse::Locations(found) = decode(&request, &measured).expect("decode") else {
            panic!("locations");
        };
        assert_eq!(found[0].uri, "file:///w/src/lib/Child.svelte");
        assert!(
            found[0].range.is_empty(),
            "an empty range is the backend naming a component, not a declaration inside one"
        );
    }

    #[test]
    fn a_shape_this_build_does_not_understand_is_an_error_not_an_empty_answer() {
        let request = SvelteRequest::Definition {
            uri: "file:///a.svelte".into(),
            position: Position::new(0, 0),
        };
        assert!(decode(&request, &json!([{ "target": "file:///t.svelte" }])).is_err());
        let SvelteResponse::Locations(found) =
            decode(&request, &Value::Null).expect("null is an answer")
        else {
            panic!("locations");
        };
        assert!(found.is_empty(), "a genuine nothing is an empty success");
    }

    #[test]
    fn generated_output_is_recognised_so_it_can_never_be_published() {
        for generated in [
            "file:///w/src/Parent.svelte.tsx",
            "file:///w/src/Parent.svelte.ts",
            "file:///w/node_modules/svelte2tsx/__sveltets_helpers.d.ts",
        ] {
            assert!(is_generated_uri(generated), "{generated}");
        }
        for real in [
            "file:///w/src/Parent.svelte",
            "file:///w/src/lib/model.ts",
            "file:///w/node_modules/svelte/index.d.ts",
        ] {
            assert!(!is_generated_uri(real), "{real}");
        }
    }

    #[test]
    fn the_connection_counts_in_utf16_because_the_server_named_nothing_else() {
        assert_eq!(POSITION_ENCODING, PositionEncoding::Utf16);
    }
}
