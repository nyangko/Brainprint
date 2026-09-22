//! The Pyright wire surface, as typed requests and answers.
//!
//! One `pyright-typeserver` process answers two protocol families on one
//! connection (#19 task 5): the `typeServer/*` requests for snapshots,
//! import resolution and types, and the LSP language-service requests
//! for definition, declaration, references and the call hierarchy. The
//! type server is a strict superset of `pyright-langserver`, so nothing
//! here launches or speaks to a second process.
//!
//! [`PythonRequest`] and [`PythonResponse`] are what travel through the
//! task 2 supervisor, which carries opaque bytes and knows no protocol.
//! They are transport, never truth: nothing in either type can become
//! canonical identity, and the adapter is the only thing that reads
//! them.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::coordinates::{Position, Range};

// ---------------------------------------------------------------------
// Protocol version
// ---------------------------------------------------------------------

/// The TSP version #19 task 5 probed and this adapter was written
/// against.
pub const TESTED_PROTOCOL_VERSION: &str = "0.4.1";

/// The compatibility class recorded in
/// [`ToolchainIdentity::backend_compatibility_class`](crate::semantic::ToolchainIdentity).
///
/// Major.minor, not the patch: TSP's own semver note says a patch stays
/// backward compatible while a 0.x *minor* bump may break, so the class
/// is exactly the range results are comparable across.
pub const COMPATIBILITY_CLASS: &str = "python-pyright-tsp:0.4";

/// Whether a server's protocol version is one this adapter may parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolCompatibility {
    /// Exactly [`TESTED_PROTOCOL_VERSION`].
    Tested,
    /// The same major.minor. Patch releases are backward compatible by
    /// the protocol's own rule.
    CompatiblePatch,
    /// Anything else, including anything unparseable. TSP is pre-1.0, so
    /// an unknown minor may have changed a shape silently -- parsing it
    /// as if it were 0.4 would turn a protocol change into wrong
    /// semantic facts. Nothing is parsed.
    Unknown,
}

impl ProtocolCompatibility {
    #[must_use]
    pub fn classify(version: &str) -> Self {
        if version == TESTED_PROTOCOL_VERSION {
            return Self::Tested;
        }
        let tested: Vec<&str> = TESTED_PROTOCOL_VERSION.split('.').collect();
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

    /// Whether the adapter may issue and parse queries at this version.
    #[must_use]
    pub const fn usable(self) -> bool {
        matches!(self, Self::Tested | Self::CompatiblePatch)
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

/// JSON-RPC `ServerCancelled`. Pyright answers a query carried on a
/// snapshot it has already invalidated with this, rather than answering
/// from the old state -- which is the currentness signal the batch
/// layer retries on.
pub const SERVER_CANCELLED: i64 = -32802;
/// JSON-RPC `MethodNotFound`. How the server reports an unsupported
/// capability: `textDocument/implementation` and
/// `textDocument/prepareTypeHierarchy` both answer this.
pub const METHOD_NOT_FOUND: i64 = -32601;

pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const INITIALIZED: &str = "initialized";
    pub const SHUTDOWN: &str = "shutdown";
    pub const EXIT: &str = "exit";
    pub const CONFIGURATION: &str = "workspace/configuration";
    pub const DID_CHANGE_WATCHED_FILES: &str = "workspace/didChangeWatchedFiles";
    pub const REGISTER_CAPABILITY: &str = "client/registerCapability";
    pub const UNREGISTER_CAPABILITY: &str = "client/unregisterCapability";
    pub const PROTOCOL_VERSION: &str = "typeServer/getSupportedProtocolVersion";
    pub const SNAPSHOT: &str = "typeServer/getSnapshot";
    pub const SNAPSHOT_CHANGED: &str = "typeServer/snapshotChanged";
    pub const RESOLVE_IMPORT: &str = "typeServer/resolveImport";
    pub const SEARCH_PATHS: &str = "typeServer/getPythonSearchPaths";
    pub const DECLARED_TYPE: &str = "typeServer/getDeclaredType";
    pub const COMPUTED_TYPE: &str = "typeServer/getComputedType";
    pub const EXPECTED_TYPE: &str = "typeServer/getExpectedType";
    pub const DEFINITION: &str = "textDocument/definition";
    pub const DECLARATION: &str = "textDocument/declaration";
    pub const TYPE_DEFINITION: &str = "textDocument/typeDefinition";
    pub const REFERENCES: &str = "textDocument/references";
    pub const PREPARE_CALL_HIERARCHY: &str = "textDocument/prepareCallHierarchy";
    pub const INCOMING_CALLS: &str = "callHierarchy/incomingCalls";
    /// Never sent. Named so the "filesystem truth, no editor overlay"
    /// decision is checkable rather than only documented.
    pub const DID_OPEN: &str = "textDocument/didOpen";
    /// Never sent. See [`DID_OPEN`].
    pub const DID_CHANGE: &str = "textDocument/didChange";
}

/// The document-synchronization notifications this adapter must never
/// emit.
///
/// #19 task 5 measured what they do: `didOpen` installs an in-memory
/// overlay that overrides the file on disk, and the backend then answers
/// from the overlay. Brainprint's Workspace is the one truth, so it
/// never opens a document -- the backend reads the same filesystem
/// Brainprint indexed.
pub const FORBIDDEN_SYNC_METHODS: [&str; 2] = [method::DID_OPEN, method::DID_CHANGE];

// ---------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------

/// A Python module as an import statement writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleDescriptor {
    /// `from . import x` is one dot, `from ..pkg import x` is two.
    pub leading_dots: u32,
    pub name_parts: Vec<String>,
}

impl ModuleDescriptor {
    /// Read a descriptor out of the text an import site writes.
    ///
    /// `None` when the text is not a module path -- which is how a
    /// symbol import site (`Base` in `from .base import Base`) is told
    /// apart from a module one without guessing from the name. A bare
    /// identifier is deliberately *not* a descriptor: `from pkg import
    /// json` writes one, and resolving it as the top-level `json`
    /// module would be a confident wrong answer.
    #[must_use]
    pub fn parse_module_site(text: &str) -> Option<Self> {
        let dots = text.len() - text.trim_start_matches('.').len();
        let rest = &text[dots..];
        if dots == 0 && !rest.contains('.') {
            return None;
        }
        let parts: Vec<String> = if rest.is_empty() {
            Vec::new()
        } else {
            rest.split('.').map(str::to_owned).collect()
        };
        if parts.iter().any(|part| !is_identifier(part)) {
            return None;
        }
        Some(Self {
            leading_dots: u32::try_from(dots).ok()?,
            name_parts: parts,
        })
    }
}

fn is_identifier(text: &str) -> bool {
    let mut characters = text.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_alphabetic())
        && characters.all(|character| character == '_' || character.is_alphanumeric())
}

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

/// One question for the Python backend.
///
/// Carried through [`RuntimeRequest::payload`](crate::runtime::RuntimeRequest)
/// as JSON, so the supervisor stays backend-neutral.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PythonRequest {
    ProtocolVersion,
    Snapshot,
    SearchPaths {
        from_uri: String,
        snapshot: u64,
    },
    ResolveImport {
        source_uri: String,
        module: ModuleDescriptor,
        snapshot: u64,
    },
    DeclaredType {
        uri: String,
        range: Range,
        snapshot: u64,
    },
    ComputedType {
        uri: String,
        range: Range,
        snapshot: u64,
    },
    ExpectedType {
        uri: String,
        range: Range,
        snapshot: u64,
    },
    Definition {
        uri: String,
        position: Position,
    },
    Declaration {
        uri: String,
        position: Position,
    },
    TypeDefinition {
        uri: String,
        position: Position,
    },
    References {
        uri: String,
        position: Position,
        include_declaration: bool,
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
    WatchedFilesChanged {
        changes: Vec<WatchedChange>,
    },
}

impl PythonRequest {
    /// The wire method, and the parameters to send with it.
    #[must_use]
    pub fn wire(&self) -> (&'static str, Option<Value>) {
        match self {
            Self::ProtocolVersion => (method::PROTOCOL_VERSION, None),
            Self::Snapshot => (method::SNAPSHOT, None),
            Self::SearchPaths { from_uri, snapshot } => (
                method::SEARCH_PATHS,
                Some(json!({ "fromUri": from_uri, "snapshot": snapshot })),
            ),
            Self::ResolveImport {
                source_uri,
                module,
                snapshot,
            } => (
                method::RESOLVE_IMPORT,
                Some(json!({
                    "sourceUri": source_uri,
                    "moduleDescriptor": {
                        "leadingDots": module.leading_dots,
                        "nameParts": module.name_parts,
                    },
                    "snapshot": snapshot,
                })),
            ),
            Self::DeclaredType {
                uri,
                range,
                snapshot,
            } => (
                method::DECLARED_TYPE,
                Some(type_params(uri, *range, *snapshot)),
            ),
            Self::ComputedType {
                uri,
                range,
                snapshot,
            } => (
                method::COMPUTED_TYPE,
                Some(type_params(uri, *range, *snapshot)),
            ),
            Self::ExpectedType {
                uri,
                range,
                snapshot,
            } => (
                method::EXPECTED_TYPE,
                Some(type_params(uri, *range, *snapshot)),
            ),
            Self::Definition { uri, position } => {
                (method::DEFINITION, Some(document_position(uri, *position)))
            }
            Self::Declaration { uri, position } => {
                (method::DECLARATION, Some(document_position(uri, *position)))
            }
            Self::TypeDefinition { uri, position } => (
                method::TYPE_DEFINITION,
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
            Self::PrepareCallHierarchy { uri, position } => (
                method::PREPARE_CALL_HIERARCHY,
                Some(document_position(uri, *position)),
            ),
            Self::IncomingCalls { item } => (method::INCOMING_CALLS, Some(json!({ "item": item }))),
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
    #[must_use]
    pub const fn is_notification(&self) -> bool {
        matches!(self, Self::WatchedFilesChanged { .. })
    }

    /// The snapshot this question is carried on, if any. A request
    /// without one cannot go stale.
    #[must_use]
    pub const fn snapshot(&self) -> Option<u64> {
        match self {
            Self::SearchPaths { snapshot, .. }
            | Self::ResolveImport { snapshot, .. }
            | Self::DeclaredType { snapshot, .. }
            | Self::ComputedType { snapshot, .. }
            | Self::ExpectedType { snapshot, .. } => Some(*snapshot),
            _ => None,
        }
    }
}

fn type_params(uri: &str, range: Range, snapshot: u64) -> Value {
    json!({
        "arg": { "uri": uri, "range": range_json(range) },
        "snapshot": snapshot,
    })
}

fn document_position(uri: &str, position: Position) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "position": { "line": position.line, "character": position.character },
    })
}

fn range_json(range: Range) -> Value {
    json!({
        "start": { "line": range.start.line, "character": range.start.character },
        "end": { "line": range.end.line, "character": range.end.character },
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
    let text = path.to_string_lossy().replace('\\', "/");
    let mut encoded = String::with_capacity(text.len() + 8);
    if !text.starts_with('/') {
        // A Windows drive path becomes `file:///C:/...`.
        encoded.push('/');
    }
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                encoded.push(char::from(byte));
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    format!("file://{encoded}")
}

/// The absolute path a `file:` URI names, or `None` for any other
/// scheme or an undecodable escape.
#[must_use]
pub fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // An authority component (`file://host/path`) names another
    // machine's file, which is not something this Workspace owns.
    let rest = match rest.strip_prefix('/') {
        Some(path) => path,
        None if rest.is_empty() => "",
        None => return None,
    };
    let mut bytes = Vec::with_capacity(rest.len());
    let mut characters = rest.bytes();
    while let Some(byte) = characters.next() {
        if byte == b'%' {
            let high = characters.next()?;
            let low = characters.next()?;
            let pair = [high, low];
            let text = std::str::from_utf8(&pair).ok()?;
            bytes.push(u8::from_str_radix(text, 16).ok()?);
        } else {
            bytes.push(byte);
        }
    }
    let decoded = String::from_utf8(bytes).ok()?;
    // `/C:/x` came from a Windows drive path; everything else is a
    // POSIX absolute path whose leading slash was consumed above.
    let has_drive = decoded
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphabetic)
        && decoded.as_bytes().get(1) == Some(&b':');
    Some(std::path::PathBuf::from(if has_drive {
        decoded
    } else {
        format!("/{decoded}")
    }))
}

// ---------------------------------------------------------------------
// Answers
// ---------------------------------------------------------------------

/// One place in one document, as the backend names it.
///
/// A URI and a UTF-16 range: a locator, not identity. The adapter
/// resolves it to a [`ResourceId`](brainprint_core::ResourceId) and a
/// byte span, and only that crosses into Brainprint truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

/// What a `typeServer/get*Type` request established.
///
/// Only the parts that can become a target. The backend's display
/// strings are deliberately absent: a name like `"Base"` is not a
/// target, and having it here would invite resolving one by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeAnswer {
    /// Where the type is declared, when it has a source declaration.
    pub declaration: Option<Location>,
    /// Set instead of `declaration` when the type *is* a module.
    pub module_name: Option<String>,
    pub module_uri: Option<String>,
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
pub enum PythonResponse {
    ProtocolVersion(String),
    Snapshot(u64),
    SearchPaths(Vec<String>),
    /// `None` when the module does not resolve. Not an error: "no such
    /// module" is an answer.
    Import(Option<String>),
    Type(Option<TypeAnswer>),
    Locations(Vec<Location>),
    /// Opaque call-hierarchy items, to be handed straight back.
    CallHierarchyItems(Vec<Value>),
    IncomingCalls(Vec<IncomingCall>),
    /// The backend refused the request because its snapshot had already
    /// moved on. A protocol answer, not a failure: the whole batch is
    /// retried on a fresh snapshot.
    SnapshotStale,
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

impl PythonRequest {
    /// Read the backend's JSON result for this request.
    pub fn decode(&self, result: &Value) -> Result<PythonResponse, DecodeError> {
        let (wire, _) = self.wire();
        let bad = |detail: String| DecodeError {
            method: wire,
            detail,
        };
        match self {
            Self::ProtocolVersion => result
                .as_str()
                .map(|version| PythonResponse::ProtocolVersion(version.to_owned()))
                .ok_or_else(|| bad("expected a version string".to_owned())),
            Self::Snapshot => result
                .as_u64()
                .map(PythonResponse::Snapshot)
                .ok_or_else(|| bad("expected a snapshot number".to_owned())),
            Self::SearchPaths { .. } => Ok(PythonResponse::SearchPaths(
                result
                    .as_array()
                    .map(|paths| {
                        paths
                            .iter()
                            .filter_map(|path| path.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
            )),
            Self::ResolveImport { .. } => {
                Ok(PythonResponse::Import(result.as_str().map(str::to_owned)))
            }
            Self::DeclaredType { .. } | Self::ComputedType { .. } | Self::ExpectedType { .. } => {
                Ok(PythonResponse::Type(decode_type(result)))
            }
            Self::Definition { .. }
            | Self::Declaration { .. }
            | Self::TypeDefinition { .. }
            | Self::References { .. } => Ok(PythonResponse::Locations(decode_locations(result))),
            Self::PrepareCallHierarchy { .. } => Ok(PythonResponse::CallHierarchyItems(
                result.as_array().cloned().unwrap_or_default(),
            )),
            Self::IncomingCalls { .. } => {
                Ok(PythonResponse::IncomingCalls(decode_incoming(result)))
            }
            Self::WatchedFilesChanged { .. } => Ok(PythonResponse::Delivered),
        }
    }
}

fn decode_type(result: &Value) -> Option<TypeAnswer> {
    let object = result.as_object()?;
    let declaration = object
        .get("declaration")
        .and_then(|declaration| declaration.get("node"))
        .and_then(decode_location);
    let module_name = object
        .get("moduleName")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let module_uri = object.get("uri").and_then(Value::as_str).map(str::to_owned);
    if declaration.is_none() && module_name.is_none() {
        // A type with neither a declaration nor a module identity says
        // nothing a target can be built from.
        return None;
    }
    Some(TypeAnswer {
        declaration,
        module_name,
        module_uri,
    })
}

/// `textDocument/definition` may answer one location, an array, or null,
/// and a `LocationLink` instead of a `Location`. All four shapes are
/// read; anything else contributes nothing rather than a wrong span.
fn decode_locations(result: &Value) -> Vec<Location> {
    match result {
        Value::Array(items) => items.iter().filter_map(decode_location).collect(),
        Value::Object(_) => decode_location(result).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn decode_location(value: &Value) -> Option<Location> {
    let uri = value
        .get("uri")
        .or_else(|| value.get("targetUri"))?
        .as_str()?
        .to_owned();
    let range = value
        .get("range")
        .or_else(|| value.get("targetSelectionRange"))
        .or_else(|| value.get("targetRange"))
        .and_then(decode_range)?;
    Some(Location { uri, range })
}

fn decode_range(value: &Value) -> Option<Range> {
    Some(Range {
        start: decode_position(value.get("start")?)?,
        end: decode_position(value.get("end")?)?,
    })
}

fn decode_position(value: &Value) -> Option<Position> {
    Some(Position {
        line: u32::try_from(value.get("line")?.as_u64()?).ok()?,
        character: u32::try_from(value.get("character")?.as_u64()?).ok()?,
    })
}

fn decode_incoming(result: &Value) -> Vec<IncomingCall> {
    let Some(items) = result.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let from = item.get("from").and_then(decode_location)?;
            let from_ranges = item
                .get("fromRanges")
                .and_then(Value::as_array)
                .map(|ranges| ranges.iter().filter_map(decode_range).collect())
                .unwrap_or_default();
            Some(IncomingCall { from, from_ranges })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_compatibility_accepts_only_the_probed_minor() {
        assert_eq!(
            ProtocolCompatibility::classify("0.4.1"),
            ProtocolCompatibility::Tested
        );
        assert_eq!(
            ProtocolCompatibility::classify("0.4.7"),
            ProtocolCompatibility::CompatiblePatch
        );
        for unknown in ["0.5.0", "0.3.9", "1.0.0", "", "0.4", "abc", "0.4.x"] {
            assert_eq!(
                ProtocolCompatibility::classify(unknown),
                ProtocolCompatibility::Unknown,
                "{unknown} must not be parsed as 0.4"
            );
            assert!(!ProtocolCompatibility::classify(unknown).usable());
        }
        assert!(ProtocolCompatibility::classify("0.4.1").usable());
        assert!(ProtocolCompatibility::classify("0.4.9").usable());
    }

    #[test]
    fn a_module_site_is_recognized_without_guessing_from_the_name() {
        assert_eq!(
            ModuleDescriptor::parse_module_site(".base"),
            Some(ModuleDescriptor {
                leading_dots: 1,
                name_parts: vec!["base".to_owned()]
            })
        );
        assert_eq!(
            ModuleDescriptor::parse_module_site("pkg.base"),
            Some(ModuleDescriptor {
                leading_dots: 0,
                name_parts: vec!["pkg".to_owned(), "base".to_owned()]
            })
        );
        assert_eq!(
            ModuleDescriptor::parse_module_site(".."),
            Some(ModuleDescriptor {
                leading_dots: 2,
                name_parts: Vec::new()
            })
        );
        // A bare identifier is ambiguous -- `import json` and
        // `from pkg import json` write the same text -- so it is not
        // resolved as a module.
        assert_eq!(ModuleDescriptor::parse_module_site("json"), None);
        assert_eq!(ModuleDescriptor::parse_module_site("Base"), None);
        assert_eq!(ModuleDescriptor::parse_module_site("a..b"), None);
        assert_eq!(ModuleDescriptor::parse_module_site("2.bad"), None);
    }

    #[test]
    fn requests_render_the_methods_task_five_probed() {
        let (method, params) = PythonRequest::ResolveImport {
            source_uri: "file:///w/pkg/impl.py".to_owned(),
            module: ModuleDescriptor {
                leading_dots: 1,
                name_parts: vec!["base".to_owned()],
            },
            snapshot: 7,
        }
        .wire();
        assert_eq!(method, "typeServer/resolveImport");
        let params = params.expect("params");
        assert_eq!(params["moduleDescriptor"]["leadingDots"], 1);
        assert_eq!(params["snapshot"], 7);

        let (method, params) = PythonRequest::Definition {
            uri: "file:///w/pkg/impl.py".to_owned(),
            position: Position::new(11, 15),
        }
        .wire();
        assert_eq!(method, "textDocument/definition");
        assert_eq!(params.expect("params")["position"]["character"], 15);
    }

    #[test]
    fn no_request_can_render_a_document_sync_notification() {
        // The filesystem-truth decision is structural: there is no
        // variant that produces didOpen or didChange.
        let every = [
            PythonRequest::ProtocolVersion,
            PythonRequest::Snapshot,
            PythonRequest::SearchPaths {
                from_uri: "file:///w".to_owned(),
                snapshot: 1,
            },
            PythonRequest::ResolveImport {
                source_uri: "file:///w/a.py".to_owned(),
                module: ModuleDescriptor {
                    leading_dots: 0,
                    name_parts: vec!["a".to_owned(), "b".to_owned()],
                },
                snapshot: 1,
            },
            PythonRequest::DeclaredType {
                uri: "file:///w/a.py".to_owned(),
                range: Range::new(Position::new(0, 0), Position::new(0, 1)),
                snapshot: 1,
            },
            PythonRequest::ComputedType {
                uri: "file:///w/a.py".to_owned(),
                range: Range::new(Position::new(0, 0), Position::new(0, 1)),
                snapshot: 1,
            },
            PythonRequest::ExpectedType {
                uri: "file:///w/a.py".to_owned(),
                range: Range::new(Position::new(0, 0), Position::new(0, 1)),
                snapshot: 1,
            },
            PythonRequest::Definition {
                uri: "file:///w/a.py".to_owned(),
                position: Position::new(0, 0),
            },
            PythonRequest::Declaration {
                uri: "file:///w/a.py".to_owned(),
                position: Position::new(0, 0),
            },
            PythonRequest::TypeDefinition {
                uri: "file:///w/a.py".to_owned(),
                position: Position::new(0, 0),
            },
            PythonRequest::References {
                uri: "file:///w/a.py".to_owned(),
                position: Position::new(0, 0),
                include_declaration: false,
            },
            PythonRequest::PrepareCallHierarchy {
                uri: "file:///w/a.py".to_owned(),
                position: Position::new(0, 0),
            },
            PythonRequest::IncomingCalls { item: json!({}) },
            PythonRequest::WatchedFilesChanged {
                changes: vec![WatchedChange {
                    uri: "file:///w/a.py".to_owned(),
                    kind: WatchedChangeKind::Changed,
                }],
            },
        ];
        for request in &every {
            let (method, _) = request.wire();
            assert!(
                !FORBIDDEN_SYNC_METHODS.contains(&method),
                "{method} installs an editor overlay over Workspace truth"
            );
        }
        assert_eq!(every.len(), 14, "every variant is covered");
    }

    #[test]
    fn a_payload_round_trips_through_the_backend_neutral_supervisor() {
        let request = PythonRequest::ComputedType {
            uri: "file:///w/pkg/impl.py".to_owned(),
            range: Range::new(Position::new(1, 2), Position::new(1, 6)),
            snapshot: 42,
        };
        let bytes = serde_json::to_vec(&request).expect("encode");
        let back: PythonRequest = serde_json::from_slice(&bytes).expect("decode");
        assert_eq!(back, request);
    }

    #[test]
    fn definition_answers_decode_from_every_shape_pyright_uses() {
        let request = PythonRequest::Definition {
            uri: "file:///w/a.py".to_owned(),
            position: Position::new(0, 0),
        };
        let single = json!({"uri": "file:///w/b.py", "range": {"start": {"line": 1, "character": 8}, "end": {"line": 1, "character": 11}}});
        assert_eq!(
            request.decode(&single),
            Ok(PythonResponse::Locations(vec![Location {
                uri: "file:///w/b.py".to_owned(),
                range: Range::new(Position::new(1, 8), Position::new(1, 11)),
            }]))
        );
        assert_eq!(
            request.decode(&json!([single.clone(), single])),
            Ok(PythonResponse::Locations(vec![
                Location {
                    uri: "file:///w/b.py".to_owned(),
                    range: Range::new(Position::new(1, 8), Position::new(1, 11)),
                };
                2
            ]))
        );
        assert_eq!(
            request.decode(&Value::Null),
            Ok(PythonResponse::Locations(Vec::new()))
        );
        let link = json!([{
            "targetUri": "file:///w/b.py",
            "targetSelectionRange": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 4}},
        }]);
        assert_eq!(
            request.decode(&link),
            Ok(PythonResponse::Locations(vec![Location {
                uri: "file:///w/b.py".to_owned(),
                range: Range::new(Position::new(2, 0), Position::new(2, 4)),
            }]))
        );
    }

    #[test]
    fn a_type_answer_keeps_only_what_a_target_can_be_built_from() {
        let request = PythonRequest::DeclaredType {
            uri: "file:///w/a.py".to_owned(),
            range: Range::new(Position::new(0, 0), Position::new(0, 4)),
            snapshot: 1,
        };
        // The class shape task 5 observed, plus a display name that must
        // not survive decoding.
        let class = json!({
            "id": 0, "kind": 3, "flags": 5,
            "declaration": {
                "kind": 0, "category": 6,
                "node": {"uri": "file:///w/base.py", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 2, "character": 11}}},
                "name": "Base",
            },
        });
        let Ok(PythonResponse::Type(Some(answer))) = request.decode(&class) else {
            panic!("a class type decodes");
        };
        assert_eq!(
            answer.declaration.as_ref().map(|node| node.uri.as_str()),
            Some("file:///w/base.py")
        );
        assert_eq!(answer.module_name, None);

        let module = json!({"id": 0, "kind": 5, "flags": 5, "moduleName": "json", "uri": "file:///stub/json/__init__.pyi"});
        let Ok(PythonResponse::Type(Some(answer))) = request.decode(&module) else {
            panic!("a module type decodes");
        };
        assert_eq!(answer.module_name.as_deref(), Some("json"));
        assert!(answer.declaration.is_none());

        assert_eq!(request.decode(&Value::Null), Ok(PythonResponse::Type(None)));
        // A type that names nothing resolvable is not a target.
        assert_eq!(
            request.decode(&json!({"id": 0, "kind": 1, "flags": 0})),
            Ok(PythonResponse::Type(None))
        );
    }

    #[test]
    fn incoming_calls_keep_their_exact_call_site_ranges() {
        let request = PythonRequest::IncomingCalls { item: json!({}) };
        let answer = json!([{
            "from": {"name": "call", "uri": "file:///w/impl.py", "range": {"start": {"line": 10, "character": 4}, "end": {"line": 10, "character": 8}}},
            "fromRanges": [{"start": {"line": 11, "character": 13}, "end": {"line": 11, "character": 16}}],
        }]);
        let Ok(PythonResponse::IncomingCalls(calls)) = request.decode(&answer) else {
            panic!("incoming calls decode");
        };
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].from.uri, "file:///w/impl.py");
        assert_eq!(
            calls[0].from_ranges,
            vec![Range::new(Position::new(11, 13), Position::new(11, 16))]
        );
    }

    #[test]
    fn only_snapshot_carrying_requests_can_go_stale() {
        assert_eq!(
            PythonRequest::ResolveImport {
                source_uri: "file:///w/a.py".to_owned(),
                module: ModuleDescriptor {
                    leading_dots: 1,
                    name_parts: vec!["b".to_owned()]
                },
                snapshot: 9,
            }
            .snapshot(),
            Some(9)
        );
        assert_eq!(
            PythonRequest::Definition {
                uri: "file:///w/a.py".to_owned(),
                position: Position::new(0, 0),
            }
            .snapshot(),
            None
        );
    }
}
