//! The TypeScript 7 native LSP wire surface, as typed requests and
//! answers.
//!
//! #19 task 10 opened with an executable transport probe, because the
//! design issue was written while TypeScript was mid-way through its
//! native port and named a `tsserver` family that is no longer the
//! product. What the probe measured on a project-local `typescript`
//! artifact:
//!
//! ```text
//! package        typescript@7.0.2
//! executable     node_modules/@typescript/typescript-<os>-<arch>/lib/tsc
//! launch         <exe> --lsp --stdio
//! serverInfo     { name: "typescript-go", version: "7.0.2" }
//! transport      Content-Length framed JSON-RPC over stdio, standard LSP
//! ```
//!
//! That is one process answering every P0 capability this tier needs --
//! `definition`, `typeDefinition`, **`implementation`**, `references`,
//! `documentSymbol`, `hover`, `signatureHelp` and the call hierarchy --
//! so Candidate B, a legacy compatibility `tsserver`, was never
//! implemented. The selection rule in the task is explicit that a second
//! transport needs measured evidence the first cannot satisfy the
//! contract, and there is none: see [`super`] for the capability record.
//!
//! [`TypeScriptRequest`] and [`TypeScriptResponse`] are what travel
//! through the task 2 supervisor, which carries opaque bytes and knows
//! no protocol. They are transport, never truth: nothing in either type
//! can become canonical identity, and the adapter is the only thing that
//! reads them.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::lsp::coordinates::{Position, PositionEncoding, Range};

// ---------------------------------------------------------------------
// Backend identity and compatibility
// ---------------------------------------------------------------------

/// The `serverInfo.name` the probe measured, and the only one this
/// adapter was written against.
///
/// The native port reports its own repository name rather than
/// "typescript", and a differently named server behind the same
/// `--lsp --stdio` command line is a different implementation whose
/// response shapes nothing here has checked.
pub const TESTED_SERVER_NAME: &str = "typescript-go";

/// The `typescript` package version #19 task 10 probed and every
/// measurement in this module was taken against.
pub const TESTED_BACKEND_VERSION: &str = "7.0.2";

/// The compatibility class recorded in
/// [`ToolchainIdentity::backend_compatibility_class`](crate::semantic::ToolchainIdentity).
///
/// Major.minor, not the patch. TypeScript's line is semver and a patch
/// release does not change what a language service answers, so results
/// stay comparable across `7.0.x`. A `7.1` is deliberately *outside* the
/// class: the task is explicit that future 7.x native LSP versions must
/// not be assumed protocol-compatible with this adapter, and a minor
/// release is where a response shape could move.
pub const COMPATIBILITY_CLASS: &str = "typescript-native-lsp:7.0";

/// Whether a server this adapter connected to is one it may parse.
///
/// Deliberately a separate, smaller question from
/// [`BackendCompatibility`](crate::semantic_index::BackendCompatibility),
/// which task 3 already owns and this task does not duplicate. That one
/// compares two *analysis profiles* to decide what happens to a stored
/// publication. This one is the gate before any of that: may this build
/// read this process's answers at all? The mapping between them is
/// total and is [`Self::publication_verdict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolCompatibility {
    /// Exactly the measured server: `typescript-go` at
    /// [`TESTED_BACKEND_VERSION`].
    Tested,
    /// The same server at another patch inside [`COMPATIBILITY_CLASS`].
    /// Backward compatible by semver, and inside the class results are
    /// comparable.
    CompatiblePatch,
    /// Anything else: another minor or major, another server name, or a
    /// version string that does not parse. Nothing is parsed. A `7.1`
    /// native LSP may have moved a response shape, and reading it as if
    /// it were `7.0` would turn a protocol change into wrong semantic
    /// facts rather than into an honest gap.
    Unknown,
}

impl ProtocolCompatibility {
    /// Classify a server from what its own `initialize` reported.
    ///
    /// Both halves matter. A name check without a version would let a
    /// future major through; a version check without a name would let
    /// any server that happens to call itself `7.0.2` through.
    #[must_use]
    pub fn classify(server_name: &str, version: &str) -> Self {
        if server_name != TESTED_SERVER_NAME {
            return Self::Unknown;
        }
        if version == TESTED_BACKEND_VERSION {
            return Self::Tested;
        }
        let tested: Vec<&str> = TESTED_BACKEND_VERSION.split('.').collect();
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

    /// Whether the adapter may issue and parse queries against this
    /// server.
    #[must_use]
    pub const fn usable(self) -> bool {
        matches!(self, Self::Tested | Self::CompatiblePatch)
    }

    /// What this verdict means for a publication already stored.
    ///
    /// The task requires that the backend compatibility policy map into
    /// task 3's existing model rather than introduce a second enum, and
    /// that an unknown shape fail toward degradation. Both hold here:
    /// a usable server keeps the question open for the profile
    /// comparison to answer
    /// ([`Compatible`](crate::semantic_index::BackendCompatibility::Compatible)),
    /// and an unrecognised one is
    /// [`Unknown`](crate::semantic_index::BackendCompatibility::Unknown),
    /// which task 3 already fails toward revalidation rather than
    /// toward "current".
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
// Wire constants
// ---------------------------------------------------------------------

/// JSON-RPC `MethodNotFound`. How a server reports a method it does not
/// implement.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC `InvalidRequest`. What the measured server answers to a
/// method outside its surface -- an error, which is the point: a
/// nonsense request cannot arrive as an empty success and become a
/// "nothing found" semantic fact.
pub const INVALID_REQUEST: i64 = -32600;
/// JSON-RPC `RequestCancelled`. Defined so a server that *does* honour
/// `$/cancelRequest` is recognised. The measured 7.0.2 does not: see
/// [`CANCELLATION_HONOURED`].
pub const REQUEST_CANCELLED: i64 = -32800;

/// Whether the selected backend honours `$/cancelRequest`.
///
/// Measured false. The probe sent a `textDocument/references` request
/// immediately followed by `$/cancelRequest` for its id, and the server
/// answered the full result rather than `RequestCancelled`. This is
/// recorded rather than worked around: task 10 acceptance asks that
/// cancellation "works or is honestly classified", and the honest
/// classification is that the in-flight request runs to completion.
///
/// The consequence is bounded and is *not* a correctness hole. The task
/// 2 supervisor's own [`CancelToken`](crate::runtime::CancelToken) still
/// stops the caller waiting, still stops the answer being normalized,
/// and still stops an obsolete result becoming a publication; only the
/// backend's CPU is not reclaimed early. A timeout therefore leaves the
/// runtime usable rather than poisoned -- the connection is still in
/// step, because the late answer is matched by id and dropped.
pub const CANCELLATION_HONOURED: bool = false;

/// The process exit code the measured server returns after a clean
/// `shutdown`/`exit`.
///
/// LSP says a server should exit `0` after an explicit `shutdown`. The
/// measured 7.0.2 exits `1` and writes `context canceled` to stderr.
/// Recorded as a constant rather than absorbed silently, because the
/// alternative is a launcher that reports every orderly stop as a crash
/// and burns the restart budget on nothing.
pub const CLEAN_EXIT_CODE: i32 = 1;

pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const INITIALIZED: &str = "initialized";
    pub const SHUTDOWN: &str = "shutdown";
    pub const EXIT: &str = "exit";
    pub const CANCEL: &str = "$/cancelRequest";
    /// Server → client. Must be answered: the measured server asks for
    /// four configuration sections during `initialized` and the probe
    /// showed it waiting on the reply.
    pub const CONFIGURATION: &str = "workspace/configuration";
    /// Server → client. Must be answered. This is how the server takes
    /// over file watching; see [`super::WATCHER_DECISION`].
    pub const REGISTER_CAPABILITY: &str = "client/registerCapability";
    /// Server → client. Must be answered.
    pub const UNREGISTER_CAPABILITY: &str = "client/unregisterCapability";
    pub const DID_CHANGE_WATCHED_FILES: &str = "workspace/didChangeWatchedFiles";
    pub const DID_CHANGE_CONFIGURATION: &str = "workspace/didChangeConfiguration";
    pub const DEFINITION: &str = "textDocument/definition";
    pub const TYPE_DEFINITION: &str = "textDocument/typeDefinition";
    pub const IMPLEMENTATION: &str = "textDocument/implementation";
    pub const REFERENCES: &str = "textDocument/references";
    pub const HOVER: &str = "textDocument/hover";
    pub const SIGNATURE_HELP: &str = "textDocument/signatureHelp";
    pub const DOCUMENT_SYMBOL: &str = "textDocument/documentSymbol";
    pub const PREPARE_CALL_HIERARCHY: &str = "textDocument/prepareCallHierarchy";
    pub const INCOMING_CALLS: &str = "callHierarchy/incomingCalls";
    pub const OUTGOING_CALLS: &str = "callHierarchy/outgoingCalls";
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
/// Unlike Python, where the policy was inherited, #19 task 10 measured
/// it. The probe ran the full change fixture three ways against 7.0.2 --
/// server-side watching, client-side `didChangeWatchedFiles`, and a
/// document overlay via `didOpen`/`didChange` -- across a new export on
/// an existing module, a newly created module, and a deletion. All three
/// were correct, including with *no* settle delay at all, so the overlay
/// bought nothing and cost the one thing that matters: an editor buffer
/// would have become a second reality alongside the Workspace
/// filesystem Brainprint indexed.
///
/// See [`super::WATCHER_DECISION`] for why the middle option won over
/// the server's own watcher.
pub const FORBIDDEN_SYNC_METHODS: [&str; 3] =
    [method::DID_OPEN, method::DID_CHANGE, method::DID_CLOSE];

/// The `positionEncoding` values this client offers, best first.
///
/// UTF-8 first is not a preference, it is the elimination of a whole
/// error class: Brainprint counts bytes, so when the server accepts
/// UTF-8 the conversion in [`LineMap`](crate::lsp::coordinates::LineMap)
/// becomes the identity and cannot land on the wrong symbol. The
/// measured 7.0.2 accepts it. UTF-16 stays in the list because it is the
/// protocol default and a server that ignores the offer means it.
pub const OFFERED_ENCODINGS: [PositionEncoding; 2] =
    [PositionEncoding::Utf8, PositionEncoding::Utf16];

/// The encoding in force on one connection, and where it came from.
///
/// The distinction is not bookkeeping. LSP says a server that reports no
/// `positionEncoding` means UTF-16, so both cases produce a usable
/// encoding -- but only one of them is something the server *said*. A
/// server that names an encoding this build cannot count in is a third
/// case, and it is a refusal: reading its positions as UTF-16 would
/// place every span at a plausible wrong offset, which is the exact
/// failure [`crate::lsp::coordinates`] exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionEncodingChoice {
    encoding: PositionEncoding,
    negotiated: bool,
}

impl PositionEncodingChoice {
    /// The server named this encoding in its `initialize` result.
    #[must_use]
    pub const fn negotiated(encoding: PositionEncoding) -> Self {
        Self {
            encoding,
            negotiated: true,
        }
    }

    /// The server named none, so LSP's default applies.
    #[must_use]
    pub const fn defaulted() -> Self {
        Self {
            encoding: PositionEncoding::Utf16,
            negotiated: false,
        }
    }

    /// Read the choice out of an `initialize` result, or `Err` with the
    /// unreadable name.
    pub fn from_initialize(result: &Value) -> Result<Self, String> {
        match result
            .get("capabilities")
            .and_then(|capabilities| capabilities.get("positionEncoding"))
        {
            None | Some(Value::Null) => Ok(Self::defaulted()),
            Some(Value::String(name)) => PositionEncoding::parse(name)
                .map(Self::negotiated)
                .ok_or_else(|| name.clone()),
            Some(other) => Err(other.to_string()),
        }
    }

    #[must_use]
    pub const fn encoding(self) -> PositionEncoding {
        self.encoding
    }

    /// Whether the server chose it, rather than LSP's default applying.
    #[must_use]
    pub const fn is_negotiated(self) -> bool {
        self.negotiated
    }

    /// A [`LineMap`](crate::lsp::coordinates::LineMap) that counts in
    /// this connection's encoding.
    #[must_use]
    pub fn line_map<'a>(self, text: &'a str) -> crate::lsp::coordinates::LineMap<'a> {
        crate::lsp::coordinates::LineMap::with_encoding(text, self.encoding)
    }
}

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

/// One question for the TypeScript/JavaScript backend.
///
/// Carried through [`RuntimeRequest::payload`](crate::runtime::RuntimeRequest)
/// as JSON, so the supervisor stays backend-neutral.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeScriptRequest {
    Definition {
        uri: String,
        position: Position,
    },
    TypeDefinition {
        uri: String,
        position: Position,
    },
    /// Which declarations implement the interface or abstract member at
    /// this position. Supported here, unlike the Python backend, where
    /// #19 task 5 measured `MethodNotFound` and task 7 had to derive
    /// `IMPLEMENTS` instead.
    Implementation {
        uri: String,
        position: Position,
    },
    References {
        uri: String,
        position: Position,
        include_declaration: bool,
    },
    /// The selected signature at a call site, as prose the server
    /// composed.
    ///
    /// Read for exactly one thing -- see [`SignatureText`] -- and never
    /// for a target. A displayed type name is not a declaration and
    /// resolving one by name is the failure mode this tier exists to
    /// prevent.
    Hover {
        uri: String,
        position: Position,
    },
    /// The whole overload set at a call site, in declaration order,
    /// with the server's own active index.
    SignatureHelp {
        uri: String,
        position: Position,
    },
    DocumentSymbol {
        uri: String,
    },
    PrepareCallHierarchy {
        uri: String,
        position: Position,
    },
    /// The item comes from a previous [`Self::PrepareCallHierarchy`]
    /// answer and is echoed back verbatim, as the protocol requires. It
    /// is a backend handle and stays one: it never leaves the adapter.
    IncomingCalls {
        item: Value,
    },
    OutgoingCalls {
        item: Value,
    },
    WatchedFilesChanged {
        changes: Vec<WatchedChange>,
    },
}

impl TypeScriptRequest {
    /// The wire method, and the parameters to send with it.
    #[must_use]
    pub fn wire(&self) -> (&'static str, Option<Value>) {
        match self {
            Self::Definition { uri, position } => {
                (method::DEFINITION, Some(document_position(uri, *position)))
            }
            Self::TypeDefinition { uri, position } => (
                method::TYPE_DEFINITION,
                Some(document_position(uri, *position)),
            ),
            Self::Implementation { uri, position } => (
                method::IMPLEMENTATION,
                Some(document_position(uri, *position)),
            ),
            Self::References {
                uri,
                position,
                include_declaration,
            } => {
                let mut params = document_position(uri, *position);
                params["context"] = json!({ "includeDeclaration": include_declaration });
                (method::REFERENCES, Some(params))
            }
            Self::Hover { uri, position } => {
                (method::HOVER, Some(document_position(uri, *position)))
            }
            Self::SignatureHelp { uri, position } => {
                let mut params = document_position(uri, *position);
                // `triggerKind: 1` is Invoked: the client asked, rather
                // than a typed character triggering it. Without a
                // context the server is entitled to answer nothing.
                params["context"] = json!({ "triggerKind": 1, "isRetrigger": false });
                (method::SIGNATURE_HELP, Some(params))
            }
            Self::DocumentSymbol { uri } => (
                method::DOCUMENT_SYMBOL,
                Some(json!({ "textDocument": { "uri": uri } })),
            ),
            Self::PrepareCallHierarchy { uri, position } => (
                method::PREPARE_CALL_HIERARCHY,
                Some(document_position(uri, *position)),
            ),
            Self::IncomingCalls { item } => (method::INCOMING_CALLS, Some(json!({ "item": item }))),
            Self::OutgoingCalls { item } => (method::OUTGOING_CALLS, Some(json!({ "item": item }))),
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
    /// Load-bearing, not bookkeeping. A notification has no reply to
    /// wait for, and JSON-RPC over one ordered stdio connection delivers
    /// it strictly before anything sent after it. That ordering is the
    /// synchronization barrier this backend relies on in place of a
    /// snapshot number: see [`super::WATCHER_DECISION`].
    #[must_use]
    pub const fn is_notification(&self) -> bool {
        matches!(self, Self::WatchedFilesChanged { .. })
    }

    /// The document this question is about, when it names one.
    ///
    /// `None` for a call-hierarchy follow-up, whose document is inside
    /// the opaque item, and for a watched-file notification, which is
    /// about the project rather than a document.
    #[must_use]
    pub fn uri(&self) -> Option<&str> {
        match self {
            Self::Definition { uri, .. }
            | Self::TypeDefinition { uri, .. }
            | Self::Implementation { uri, .. }
            | Self::References { uri, .. }
            | Self::Hover { uri, .. }
            | Self::SignatureHelp { uri, .. }
            | Self::PrepareCallHierarchy { uri, .. }
            | Self::DocumentSymbol { uri } => Some(uri),
            Self::IncomingCalls { .. }
            | Self::OutgoingCalls { .. }
            | Self::WatchedFilesChanged { .. } => None,
        }
    }
}

fn document_position(uri: &str, position: Position) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "position": { "line": position.line, "character": position.character },
    })
}

// ---------------------------------------------------------------------
// File URIs
// ---------------------------------------------------------------------

/// A `file:` URI for an absolute path.
///
/// Percent-encodes everything outside the unreserved set, so a path with
/// spaces or non-ASCII characters survives the round trip. The result is
/// a locator handed to the backend, never identity: Brainprint's own
/// identity for the same file is its
/// [`ResourceId`](brainprint_core::ResourceId).
#[must_use]
pub fn path_to_uri(path: &std::path::Path) -> String {
    crate::python_semantic::protocol::path_to_uri(path)
}

/// The absolute path a `file:` URI names, or `None` for any other
/// scheme or an undecodable escape.
///
/// Percent-decoding is not optional here. The measured server answers
/// an external declaration inside a scoped package as
/// `.../node_modules/%40types/node/path.d.ts`, and treating that text as
/// a path would produce a Resource that does not exist and an
/// `ExternalEntity` keyed on a mangled name.
#[must_use]
pub fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    crate::python_semantic::protocol::uri_to_path(uri)
}

// ---------------------------------------------------------------------
// Answers
// ---------------------------------------------------------------------

/// One place in one document, as the backend names it.
///
/// A URI and a range in the negotiated encoding: a locator, not
/// identity. The adapter resolves it to a
/// [`ResourceId`](brainprint_core::ResourceId) and a byte span, and only
/// that crosses into Brainprint truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

/// The prose a `hover` answered, and nothing else from it.
///
/// The server composes a declaration signature for the symbol under the
/// cursor. At an overloaded call site that signature is the *selected*
/// overload -- the probe measured `parse("a")` hovering as
/// `function parse(value: string): StringResult` and `parse(1)` as
/// `function parse(value: number): NumberResult`. That is real proof of
/// overload selection from the public boundary, with no compiler
/// internals.
///
/// It is still only ever corroboration. The target comes from
/// `textDocument/definition`, which the same probe measured landing on
/// the matching *declaration line* of each overload. Nothing here is
/// matched against parameter text to decide a target, which the task
/// forbids outright.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureText {
    pub text: String,
    /// The span the server says the hover describes, when it says.
    pub range: Option<Range>,
}

/// An overload set as `signatureHelp` reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureSet {
    /// One label per candidate signature, in declaration order.
    pub signatures: Vec<String>,
    /// Which candidate the server selected for this call site. `None`
    /// when it did not say, which is a gap and not a default of zero:
    /// reading an absent selection as "the first overload" would
    /// manufacture exactly the confident wrong answer this tier refuses.
    pub active: Option<usize>,
}

/// One symbol as `documentSymbol` reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentSymbol {
    pub name: String,
    /// The LSP `SymbolKind` number, kept as the number it is. Meaning
    /// is assigned by the adapter, so an unknown kind stays unknown
    /// rather than being rounded to a neighbour.
    pub kind: i64,
    pub container: Option<String>,
    pub location: Location,
}

/// One caller of a call-hierarchy item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncomingCall {
    /// The calling declaration.
    pub from: Location,
    /// The exact call sites inside it.
    pub from_ranges: Vec<Range>,
}

/// What the backend answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeScriptResponse {
    Locations(Vec<Location>),
    /// `None` when the server had nothing to say at that position.
    Signature(Option<SignatureText>),
    SignatureSet(Option<SignatureSet>),
    DocumentSymbols(Vec<DocumentSymbol>),
    /// Opaque call-hierarchy items, to be handed straight back.
    CallHierarchyItems(Vec<Value>),
    IncomingCalls(Vec<IncomingCall>),
    OutgoingCalls(Vec<IncomingCall>),
    /// The backend does not implement the method. Recorded rather than
    /// turned into an empty result -- an unsupported capability is not
    /// zero findings.
    Unsupported(String),
    /// A notification was delivered. There is nothing to wait for.
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

// ---------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------

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

/// One `Location`, `LocationLink`, or neither.
///
/// The measured server answers both shapes on the same method depending
/// on whether the client declared `definition.linkSupport`: a plain
/// `{uri, range}` for a same-file target and a
/// `{targetUri, targetSelectionRange, ...}` link for an import
/// specifier. Both are read, and a third shape is a decode error rather
/// than a skipped element -- a response this build does not understand
/// must not arrive as "no definition".
fn location(value: &Value, method: &'static str) -> Result<Location, DecodeError> {
    if let (Some(uri), Some(raw)) = (value.get("uri").and_then(Value::as_str), value.get("range")) {
        return Ok(Location {
            uri: uri.to_owned(),
            range: range(raw, method)?,
        });
    }
    if let Some(uri) = value.get("targetUri").and_then(Value::as_str) {
        // The selection range names the identifier; the target range
        // names the whole declaration. Brainprint anchors on the
        // identifier, which is what selects a Symbol.
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

/// Read a `Location | Location[] | LocationLink[] | null` answer.
fn locations(result: &Value, method: &'static str) -> Result<Vec<Location>, DecodeError> {
    match result {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items
            .iter()
            .map(|item| location(item, method))
            .collect::<Result<Vec<_>, _>>(),
        single => Ok(vec![location(single, method)?]),
    }
}

/// Read what the server answered into the typed shape the request asked
/// for.
///
/// Every unexpected shape is a [`DecodeError`], never an empty success.
/// That is the whole contract of this function: task 10 acceptance
/// requires that a malformed or unsupported protocol response cannot
/// silently become a semantic fact, and the only way to guarantee it is
/// for "I could not read this" and "there is nothing there" to be
/// different values all the way up.
pub fn decode(
    request: &TypeScriptRequest,
    result: &Value,
) -> Result<TypeScriptResponse, DecodeError> {
    let (name, _) = request.wire();
    match request {
        TypeScriptRequest::Definition { .. }
        | TypeScriptRequest::TypeDefinition { .. }
        | TypeScriptRequest::Implementation { .. }
        | TypeScriptRequest::References { .. } => {
            Ok(TypeScriptResponse::Locations(locations(result, name)?))
        }
        TypeScriptRequest::Hover { .. } => {
            if result.is_null() {
                return Ok(TypeScriptResponse::Signature(None));
            }
            let contents = result
                .get("contents")
                .ok_or_else(|| bad(name, "hover without contents"))?;
            // `MarkupContent`, or the deprecated string / string[] forms.
            let text = match contents {
                Value::String(text) => text.clone(),
                Value::Object(_) => contents
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| bad(name, "hover contents without a value"))?
                    .to_owned(),
                Value::Array(parts) => parts
                    .iter()
                    .map(|part| match part {
                        Value::String(text) => Ok(text.clone()),
                        other => other
                            .get("value")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .ok_or_else(|| bad(name, "hover part without a value")),
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .join("\n"),
                _ => return Err(bad(name, "hover contents of an unknown shape")),
            };
            let span = match result.get("range") {
                None | Some(Value::Null) => None,
                Some(raw) => Some(range(raw, name)?),
            };
            Ok(TypeScriptResponse::Signature(Some(SignatureText {
                text,
                range: span,
            })))
        }
        TypeScriptRequest::SignatureHelp { .. } => {
            if result.is_null() {
                return Ok(TypeScriptResponse::SignatureSet(None));
            }
            let raw = result
                .get("signatures")
                .and_then(Value::as_array)
                .ok_or_else(|| bad(name, "signature help without signatures"))?;
            let signatures = raw
                .iter()
                .map(|item| {
                    item.get("label")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .ok_or_else(|| bad(name, "signature without a label"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let active = match result.get("activeSignature") {
                None | Some(Value::Null) => None,
                Some(value) => Some(
                    value
                        .as_u64()
                        .and_then(|index| usize::try_from(index).ok())
                        .ok_or_else(|| bad(name, "activeSignature is not an index"))?,
                ),
            };
            // An index the set does not contain is a protocol answer
            // this build cannot interpret, not a reason to fall back to
            // the first overload.
            if active.is_some_and(|index| index >= signatures.len()) {
                return Err(bad(name, "activeSignature is outside the signature set"));
            }
            Ok(TypeScriptResponse::SignatureSet(Some(SignatureSet {
                signatures,
                active,
            })))
        }
        TypeScriptRequest::DocumentSymbol { .. } => {
            let Some(items) = result.as_array() else {
                if result.is_null() {
                    return Ok(TypeScriptResponse::DocumentSymbols(Vec::new()));
                }
                return Err(bad(name, "document symbols is not an array"));
            };
            let symbols = items
                .iter()
                .map(|item| {
                    let symbol_name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| bad(name, "symbol without a name"))?
                        .to_owned();
                    let kind = item
                        .get("kind")
                        .and_then(Value::as_i64)
                        .ok_or_else(|| bad(name, "symbol without a kind"))?;
                    let raw = item
                        .get("location")
                        .ok_or_else(|| bad(name, "symbol without a location"))?;
                    Ok(DocumentSymbol {
                        name: symbol_name,
                        kind,
                        container: item
                            .get("containerName")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        location: location(raw, name)?,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(TypeScriptResponse::DocumentSymbols(symbols))
        }
        TypeScriptRequest::PrepareCallHierarchy { .. } => match result {
            Value::Null => Ok(TypeScriptResponse::CallHierarchyItems(Vec::new())),
            Value::Array(items) => Ok(TypeScriptResponse::CallHierarchyItems(items.clone())),
            _ => Err(bad(name, "call hierarchy items is not an array")),
        },
        TypeScriptRequest::IncomingCalls { .. } | TypeScriptRequest::OutgoingCalls { .. } => {
            let incoming = matches!(request, TypeScriptRequest::IncomingCalls { .. });
            let end = if incoming { "from" } else { "to" };
            let calls = match result {
                Value::Null => Vec::new(),
                Value::Array(items) => items
                    .iter()
                    .map(|item| {
                        let raw = item
                            .get(end)
                            .ok_or_else(|| bad(name, "call without an endpoint"))?;
                        let uri = raw
                            .get("uri")
                            .and_then(Value::as_str)
                            .ok_or_else(|| bad(name, "call endpoint without a uri"))?
                            .to_owned();
                        let selection = raw
                            .get("selectionRange")
                            .or_else(|| raw.get("range"))
                            .ok_or_else(|| bad(name, "call endpoint without a range"))?;
                        let ranges = item
                            .get("fromRanges")
                            .and_then(Value::as_array)
                            .map(|raw_ranges| {
                                raw_ranges
                                    .iter()
                                    .map(|one| range(one, name))
                                    .collect::<Result<Vec<_>, _>>()
                            })
                            .transpose()?
                            .unwrap_or_default();
                        Ok(IncomingCall {
                            from: Location {
                                uri,
                                range: range(selection, name)?,
                            },
                            from_ranges: ranges,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                _ => return Err(bad(name, "calls is not an array")),
            };
            Ok(if incoming {
                TypeScriptResponse::IncomingCalls(calls)
            } else {
                TypeScriptResponse::OutgoingCalls(calls)
            })
        }
        TypeScriptRequest::WatchedFilesChanged { .. } => Ok(TypeScriptResponse::Delivered),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic_index::BackendCompatibility;

    fn position_at(line: u32, character: u32) -> Value {
        json!({ "line": line, "character": character })
    }

    fn range_at(line: u32, start: u32, end: u32) -> Value {
        json!({ "start": position_at(line, start), "end": position_at(line, end) })
    }

    #[test]
    fn the_measured_server_is_the_tested_one() {
        assert_eq!(
            ProtocolCompatibility::classify(TESTED_SERVER_NAME, TESTED_BACKEND_VERSION),
            ProtocolCompatibility::Tested
        );
        assert!(ProtocolCompatibility::Tested.usable());
    }

    #[test]
    fn a_patch_inside_the_class_stays_comparable() {
        assert_eq!(
            ProtocolCompatibility::classify(TESTED_SERVER_NAME, "7.0.9"),
            ProtocolCompatibility::CompatiblePatch
        );
        assert!(ProtocolCompatibility::CompatiblePatch.usable());
        assert!(COMPATIBILITY_CLASS.ends_with("7.0"));
    }

    #[test]
    fn a_later_minor_is_not_assumed_compatible() {
        // The task is explicit: future 7.x native LSP versions must not
        // be assumed protocol-compatible with this adapter.
        for version in ["7.1.0", "7.1.0-dev.20260921.1", "8.0.0", "6.9.9"] {
            assert_eq!(
                ProtocolCompatibility::classify(TESTED_SERVER_NAME, version),
                ProtocolCompatibility::Unknown,
                "{version} must not be read as 7.0"
            );
        }
    }

    #[test]
    fn another_server_behind_the_same_command_line_is_unknown() {
        assert_eq!(
            ProtocolCompatibility::classify("tsserver", TESTED_BACKEND_VERSION),
            ProtocolCompatibility::Unknown
        );
        assert_eq!(
            ProtocolCompatibility::classify("", TESTED_BACKEND_VERSION),
            ProtocolCompatibility::Unknown
        );
    }

    #[test]
    fn an_unusable_server_fails_toward_revalidation_not_toward_current() {
        let verdict = ProtocolCompatibility::Unknown.publication_verdict();
        assert_eq!(verdict, BackendCompatibility::Unknown);
        assert!(!verdict.keeps_publication());
        assert!(
            ProtocolCompatibility::Tested
                .publication_verdict()
                .keeps_publication()
        );
    }

    #[test]
    fn document_sync_methods_are_never_a_request_this_type_can_express() {
        // The strongest form the "no editor overlay" decision can take:
        // there is no variant that emits one, so it is not a policy
        // that can be forgotten.
        let emitted: Vec<&str> = [
            TypeScriptRequest::Definition {
                uri: "file:///a.ts".into(),
                position: Position::new(0, 0),
            },
            TypeScriptRequest::Hover {
                uri: "file:///a.ts".into(),
                position: Position::new(0, 0),
            },
            TypeScriptRequest::WatchedFilesChanged {
                changes: Vec::new(),
            },
        ]
        .iter()
        .map(|request| request.wire().0)
        .collect();
        for forbidden in FORBIDDEN_SYNC_METHODS {
            assert!(!emitted.contains(&forbidden));
        }
    }

    #[test]
    fn a_watched_change_carries_the_wire_numbers_lsp_defines() {
        let request = TypeScriptRequest::WatchedFilesChanged {
            changes: vec![
                WatchedChange {
                    uri: "file:///new.ts".into(),
                    kind: WatchedChangeKind::Created,
                },
                WatchedChange {
                    uri: "file:///old.ts".into(),
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
    fn both_definition_shapes_decode_to_the_identifier_span() {
        let request = TypeScriptRequest::Definition {
            uri: "file:///a.ts".into(),
            position: Position::new(0, 0),
        };
        let plain = json!([{ "uri": "file:///t.ts", "range": range_at(2, 13, 20) }]);
        let TypeScriptResponse::Locations(found) = decode(&request, &plain).expect("plain") else {
            panic!("locations");
        };
        assert_eq!(
            found[0].range,
            Range::new(Position::new(2, 13), Position::new(2, 20))
        );

        let link = json!([{
            "targetUri": "file:///t.ts",
            "targetRange": range_at(2, 0, 40),
            "targetSelectionRange": range_at(2, 13, 20),
        }]);
        let TypeScriptResponse::Locations(found) = decode(&request, &link).expect("link") else {
            panic!("locations");
        };
        // The selection range, not the whole declaration: that is what
        // selects a Symbol.
        assert_eq!(
            found[0].range,
            Range::new(Position::new(2, 13), Position::new(2, 20))
        );
    }

    #[test]
    fn a_shape_this_build_does_not_understand_is_an_error_not_an_empty_answer() {
        let request = TypeScriptRequest::Definition {
            uri: "file:///a.ts".into(),
            position: Position::new(0, 0),
        };
        assert!(decode(&request, &json!([{ "target": "file:///t.ts" }])).is_err());
        assert!(decode(&request, &json!([{ "uri": "file:///t.ts" }])).is_err());
        // Whereas a genuine "nothing here" is an empty success.
        let TypeScriptResponse::Locations(found) =
            decode(&request, &Value::Null).expect("null is an answer")
        else {
            panic!("locations");
        };
        assert!(found.is_empty());
    }

    #[test]
    fn hover_keeps_the_selected_signature_text() {
        let request = TypeScriptRequest::Hover {
            uri: "file:///overload.ts".into(),
            position: Position::new(9, 17),
        };
        // Verbatim from the #19 task 10 probe against 7.0.2.
        let measured = json!({
            "contents": {
                "kind": "plaintext",
                "value": "function parse(value: string): StringResult",
            },
            "range": range_at(9, 17, 22),
        });
        let TypeScriptResponse::Signature(Some(signature)) =
            decode(&request, &measured).expect("hover")
        else {
            panic!("signature");
        };
        assert_eq!(
            signature.text,
            "function parse(value: string): StringResult"
        );
        assert_eq!(
            signature.range,
            Some(Range::new(Position::new(9, 17), Position::new(9, 22)))
        );
    }

    #[test]
    fn an_absent_overload_selection_does_not_become_the_first_overload() {
        let request = TypeScriptRequest::SignatureHelp {
            uri: "file:///overload.ts".into(),
            position: Position::new(9, 17),
        };
        let without = json!({
            "signatures": [
                { "label": "parse(value: string): StringResult" },
                { "label": "parse(value: number): NumberResult" },
            ],
        });
        let TypeScriptResponse::SignatureSet(Some(set)) = decode(&request, &without).expect("help")
        else {
            panic!("set");
        };
        assert_eq!(set.signatures.len(), 2);
        assert_eq!(set.active, None, "no selection is a gap, not index 0");

        let outside = json!({
            "signatures": [{ "label": "parse(value: string): StringResult" }],
            "activeSignature": 4,
        });
        assert!(
            decode(&request, &outside).is_err(),
            "an index outside the set is unreadable, not a fallback"
        );
    }

    #[test]
    fn the_measured_signature_help_decodes_with_its_selection() {
        let request = TypeScriptRequest::SignatureHelp {
            uri: "file:///overload.ts".into(),
            position: Position::new(9, 17),
        };
        let measured = json!({
            "signatures": [
                { "label": "parse(value: string): StringResult",
                  "parameters": [{ "label": "value: string" }] },
                { "label": "parse(value: number): NumberResult",
                  "parameters": [{ "label": "value: number" }] },
            ],
            "activeSignature": 0,
            "activeParameter": 0,
        });
        let TypeScriptResponse::SignatureSet(Some(set)) =
            decode(&request, &measured).expect("help")
        else {
            panic!("set");
        };
        assert_eq!(set.active, Some(0));
        assert_eq!(set.signatures[1], "parse(value: number): NumberResult");
    }

    #[test]
    fn a_scoped_package_uri_percent_decodes_back_to_its_real_path() {
        // The measured external target. Reading the raw text as a path
        // would key an ExternalEntity on `%40types`.
        let uri = "file:///w/node_modules/%40types/node/path.d.ts";
        assert_eq!(
            uri_to_path(uri).expect("path"),
            std::path::PathBuf::from("/w/node_modules/@types/node/path.d.ts")
        );
        assert_eq!(
            path_to_uri(std::path::Path::new(
                "/w/node_modules/@types/node/path.d.ts"
            )),
            uri
        );
    }

    #[test]
    fn incoming_calls_keep_every_call_site_inside_one_caller() {
        let request = TypeScriptRequest::IncomingCalls { item: json!({}) };
        let measured = json!([{
            "from": { "name": "consume", "uri": "file:///consumer.ts",
                      "range": range_at(4, 0, 9), "selectionRange": range_at(4, 16, 23) },
            "fromRanges": [range_at(9, 13, 21), range_at(11, 4, 12)],
        }]);
        let TypeScriptResponse::IncomingCalls(calls) = decode(&request, &measured).expect("calls")
        else {
            panic!("calls");
        };
        assert_eq!(calls[0].from.range.start, Position::new(4, 16));
        assert_eq!(calls[0].from_ranges.len(), 2);
    }

    #[test]
    fn utf8_is_offered_first_so_the_mapping_can_be_the_identity() {
        assert_eq!(OFFERED_ENCODINGS[0], PositionEncoding::Utf8);
        assert!(OFFERED_ENCODINGS.contains(&PositionEncoding::Utf16));
    }

    #[test]
    fn the_measured_handshake_settles_on_utf8() {
        // Verbatim shape from the #19 task 10 probe against 7.0.2.
        let result = json!({
            "capabilities": { "positionEncoding": "utf-8", "definitionProvider": true },
            "serverInfo": { "name": "typescript-go", "version": "7.0.2" },
        });
        let choice = PositionEncodingChoice::from_initialize(&result).expect("readable");
        assert_eq!(choice.encoding(), PositionEncoding::Utf8);
        assert!(choice.is_negotiated());
    }

    #[test]
    fn a_server_that_names_no_encoding_means_the_lsp_default() {
        let choice = PositionEncodingChoice::from_initialize(&json!({ "capabilities": {} }))
            .expect("readable");
        assert_eq!(choice.encoding(), PositionEncoding::Utf16);
        assert!(
            !choice.is_negotiated(),
            "the default applying is not the server having chosen"
        );
    }

    #[test]
    fn an_encoding_this_build_cannot_count_in_is_refused_not_defaulted() {
        // Silently reading these as UTF-16 would put every span at a
        // plausible wrong offset, which is worse than no span.
        for name in ["utf-7", "UTF-8", ""] {
            assert_eq!(
                PositionEncodingChoice::from_initialize(
                    &json!({ "capabilities": { "positionEncoding": name } })
                ),
                Err(name.to_owned()),
                "{name} must not be rounded to the default"
            );
        }
    }

    #[test]
    fn a_utf8_connection_maps_a_korean_line_by_bytes() {
        let choice = PositionEncodingChoice::negotiated(PositionEncoding::Utf8);
        let text = "export const 한글변수 = 1;\n";
        let map = choice.line_map(text);
        let byte = text.find('=').expect("needle");
        let position = map.position(byte).expect("position");
        assert_eq!(usize::try_from(position.character).expect("fits"), byte);
        assert_eq!(map.byte(position), Ok(byte));
    }
}
