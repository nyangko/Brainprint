//! The Roslyn language server's wire surface, as typed requests and
//! answers.
//!
//! #19 task 12 opened with a measurement rather than a design, because
//! the ecosystem moved after #19 was written: there is now an official
//! `Microsoft.CodeAnalysis.LanguageServer`, and building a private
//! Roslyn worker before measuring it would have been inventing work.
//!
//! ```text
//! package   Microsoft.CodeAnalysis.LanguageServer.<rid>  5.4.0-2.26179.14
//! feed      vs-impl (Azure DevOps) -- not nuget.org
//! launch    <exe> --stdio --logLevel <level> --extensionLogDirectory <dir>
//! serverInfo        absent -> identity comes from the package
//! positionEncoding  absent -> LSP default, UTF-16
//! ```
//!
//! `--logLevel` and `--extensionLogDirectory` are *required* arguments,
//! and there is no `--autoLoadProjects`: projects are loaded through the
//! server's own [`method::SOLUTION_OPEN`] notification.
//!
//! What the measurement settled, in the order it mattered.
//!
//! **There is a real barrier.** The server sends
//! [`method::PROJECT_INITIALIZATION_COMPLETE`] when a solution has
//! finished loading -- ~8s for the task's four-project fixture. That is
//! the deterministic signal the task demanded in place of a sleep.
//!
//! **Loading a project executes the project's own build logic.** A
//! `<Target BeforeTargets="Build">` that writes a marker file *ran*, and
//! `obj/` appeared in the project directory. There is no switch to stop
//! it, and it is not a property of this transport: a design-time build
//! is how Roslyn obtains a compiler command line, so `MSBuildWorkspace`
//! would run the same targets. That is why [`ProjectExecutionTrust`]
//! exists and why its default is [`ProjectExecutionTrust::Untrusted`].
//!
//! **An existing-file edit needs a document overlay.** Measured three
//! times in a row: after an edit on disk plus
//! `workspace/didChangeWatchedFiles`, the server kept answering the old
//! position; after [`method::DID_OPEN`] carrying the same bytes, the new
//! one. Unlike the TypeScript tier, C# therefore uses LSP document
//! synchronization -- carrying Brainprint's own current Resource bytes,
//! never an editor buffer.
//!
//! **A project-membership change needs a reload.** A new `.cs` file and
//! a reference to it was repaired by neither watched files nor a
//! document open; the Compile set comes from MSBuild. So the lifecycle
//! has two change classes, not one.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::lsp::coordinates::{Position, PositionEncoding, Range};

// ---------------------------------------------------------------------
// Trust
// ---------------------------------------------------------------------

/// Whether this Workspace may have its project build logic executed.
///
/// #19 task 12 decision 1, and the reason it is a type rather than a
/// `bool`: the answer has to be readable at every call site that could
/// start a design-time build, and `false` is not self-describing.
///
/// The default is [`Self::Untrusted`] and nothing infers otherwise. Not
/// that the repository is local, not that it is a Git checkout, not that
/// an Agent is already editing it, not that it built before, not that
/// someone opened the directory. The Roslyn backend runs in its own
/// process, and that is crash and resource isolation -- it is not a
/// security boundary, and treating it as one would be the whole mistake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectExecutionTrust {
    /// No project load. The server is driven with documents only, which
    /// was measured to execute no project code at all: the marker target
    /// did not run, and `obj/` was not created.
    ///
    /// What survives is real and bounded -- intra-document definitions
    /// and document symbols -- and everything that needs a compilation
    /// is an honest gap. Level A is not claimed.
    Untrusted,
    /// An explicit decision, made outside this tier, that this
    /// Workspace's build logic may run. Only this permits the project
    /// load that C# Level A needs.
    Trusted,
}

impl ProjectExecutionTrust {
    /// The only default there is.
    #[must_use]
    pub const fn default_for_workspace() -> Self {
        Self::Untrusted
    }

    /// Whether the project-loading path may be taken.
    #[must_use]
    pub const fn may_load_projects(self) -> bool {
        matches!(self, Self::Trusted)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Untrusted => "UNTRUSTED",
            Self::Trusted => "TRUSTED",
        }
    }
}

impl Default for ProjectExecutionTrust {
    fn default() -> Self {
        Self::default_for_workspace()
    }
}

impl fmt::Display for ProjectExecutionTrust {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What project loading was measured to execute, in one place.
///
/// Recorded rather than summarised as "isolated", because it is neither
/// sandboxed nor inert: a project-supplied MSBuild target ran, and the
/// load wrote `obj/` into the project directory.
pub const MEASURED_PROJECT_LOAD_EFFECTS: &str =
    "evaluates MSBuild, runs project-supplied targets, writes obj/; not a sandbox";

// ---------------------------------------------------------------------
// Backend identity and compatibility
// ---------------------------------------------------------------------

/// The language server version #19 task 12 measured.
pub const TESTED_SERVER_VERSION: &str = "5.4.0-2.26179.14";

/// The compatibility class recorded in
/// [`ToolchainIdentity::backend_compatibility_class`](crate::semantic::ToolchainIdentity).
///
/// Major.minor of the Roslyn line. These builds ship daily with a
/// date-stamped patch, so pinning the patch would refuse every install
/// but one; a minor is where a response shape could move.
pub const COMPATIBILITY_CLASS: &str = "roslyn-language-server:5.4";

/// Whether a located install is one this adapter may parse answers from.
///
/// Classified from the package manifest, because the measured server
/// reports no `serverInfo` -- there is nothing in the protocol to
/// identify it with, and the manifest is read before launch anyway,
/// which is what [`AnalysisContext`](crate::semantic::AnalysisContext)
/// needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolCompatibility {
    Tested,
    CompatiblePatch,
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
        if found.len() >= 2 && found[0] == tested[0] && found[1] == tested[1] {
            return Self::CompatiblePatch;
        }
        Self::Unknown
    }

    #[must_use]
    pub const fn usable(self) -> bool {
        matches!(self, Self::Tested | Self::CompatiblePatch)
    }

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

/// The position encoding this connection counts in.
///
/// UTF-16: the measured server reports no `positionEncoding`, and LSP
/// says that means UTF-16.
pub const POSITION_ENCODING: PositionEncoding = PositionEncoding::Utf16;

/// JSON-RPC `MethodNotFound`.
pub const METHOD_NOT_FOUND: i64 = -32601;

/// The server withdrew a request rather than answering it.
///
/// LSP lets a server do this when a request stops being valid, and the
/// measured one does: a `textDocument/definition` in flight while the
/// document it names is being re-analysed comes back `-32800`. It is
/// neither an answer nor a broken backend, so it gets its own response
/// -- see [`CSharpResponse::Cancelled`].
pub const REQUEST_CANCELLED: i64 = -32800;

pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const INITIALIZED: &str = "initialized";
    pub const SHUTDOWN: &str = "shutdown";
    pub const EXIT: &str = "exit";
    pub const CONFIGURATION: &str = "workspace/configuration";
    pub const REGISTER_CAPABILITY: &str = "client/registerCapability";
    pub const UNREGISTER_CAPABILITY: &str = "client/unregisterCapability";
    /// Server → client. The load barrier: sent once a solution or
    /// project set has finished loading.
    pub const PROJECT_INITIALIZATION_COMPLETE: &str = "workspace/projectInitializationComplete";
    /// Client → server. Roslyn's own project-loading notification. Only
    /// ever sent under [`super::ProjectExecutionTrust::Trusted`].
    pub const SOLUTION_OPEN: &str = "solution/open";
    /// The same, for a project set rather than a solution.
    pub const PROJECT_OPEN: &str = "project/open";
    pub const DEFINITION: &str = "textDocument/definition";
    pub const REFERENCES: &str = "textDocument/references";
    pub const IMPLEMENTATION: &str = "textDocument/implementation";
    pub const DOCUMENT_SYMBOL: &str = "textDocument/documentSymbol";
    /// Brainprint's own transport copy of current Resource bytes. See
    /// [`super::DOCUMENT_SYNC_DECISION`].
    pub const DID_OPEN: &str = "textDocument/didOpen";
    pub const DID_CHANGE: &str = "textDocument/didChange";
    pub const DID_CLOSE: &str = "textDocument/didClose";
    pub const DID_CHANGE_WATCHED_FILES: &str = "workspace/didChangeWatchedFiles";
}

/// Why this backend sends document synchronization when the TypeScript
/// one does not.
///
/// Measured, not copied. After an edit on disk plus a watched-file
/// notification, three consecutive `textDocument/definition` requests
/// returned the *old* position; after `didOpen` with the same bytes,
/// three consecutive requests returned the new one.
///
/// The bytes are Brainprint's, read from the current Resource it
/// indexed. The rule is unchanged from every other tier:
///
/// ```text
/// filesystem / current Resource = truth
/// document sync                 = a transport copy of that truth
/// ```
///
/// No unsaved editor content is ever accepted, because none is ever
/// offered: there is no editor here.
pub const DOCUMENT_SYNC_DECISION: &str =
    "Brainprint-owned document sync; watched files alone were measured stale";

// ---------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------

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

/// One question for, or statement to, the C# backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CSharpRequest {
    Definition {
        uri: String,
        position: Position,
    },
    Implementation {
        uri: String,
        position: Position,
    },
    References {
        uri: String,
        position: Position,
        include_declaration: bool,
    },
    DocumentSymbol {
        uri: String,
    },
    /// Brainprint's current bytes for one Resource.
    OpenDocument {
        uri: String,
        text: String,
        version: i64,
    },
    /// The whole document again, at a new version.
    ///
    /// `replaces` is the range the new text takes the place of: the
    /// whole of the previous document. Brainprint has current bytes and
    /// no edit stream, so this is a full replacement -- but the server
    /// declares `textDocumentSync.change: 2` (Incremental), and it does
    /// not merely ignore a change with no range: the measured one
    /// **exits** on one, taking the connection with it. So the
    /// replacement is expressed the way the server asked for it, which
    /// invents no delta: one change, spanning everything that was there.
    ChangeDocument {
        uri: String,
        text: String,
        version: i64,
        replaces: Range,
    },
    CloseDocument {
        uri: String,
    },
    /// Load a solution. Only ever sent under `Trusted`.
    OpenSolution {
        uri: String,
    },
    /// Load a project set. Only ever sent under `Trusted`.
    OpenProjects {
        uris: Vec<String>,
    },
    WatchedFilesChanged {
        changes: Vec<WatchedChange>,
    },
}

impl CSharpRequest {
    #[must_use]
    pub fn wire(&self) -> (&'static str, Option<Value>) {
        match self {
            Self::Definition { uri, position } => {
                (method::DEFINITION, Some(document_position(uri, *position)))
            }
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
            Self::DocumentSymbol { uri } => (
                method::DOCUMENT_SYMBOL,
                Some(json!({ "textDocument": { "uri": uri } })),
            ),
            Self::OpenDocument { uri, text, version } => (
                method::DID_OPEN,
                Some(json!({ "textDocument": {
                    "uri": uri, "languageId": "csharp", "version": version, "text": text,
                }})),
            ),
            Self::ChangeDocument {
                uri,
                text,
                version,
                replaces,
            } => (
                method::DID_CHANGE,
                Some(json!({
                    "textDocument": { "uri": uri, "version": version },
                    "contentChanges": [{ "range": range_value(*replaces), "text": text }],
                })),
            ),
            Self::CloseDocument { uri } => (
                method::DID_CLOSE,
                Some(json!({ "textDocument": { "uri": uri } })),
            ),
            Self::OpenSolution { uri } => (method::SOLUTION_OPEN, Some(json!({ "solution": uri }))),
            Self::OpenProjects { uris } => {
                (method::PROJECT_OPEN, Some(json!({ "projects": uris })))
            }
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
        matches!(
            self,
            Self::OpenDocument { .. }
                | Self::ChangeDocument { .. }
                | Self::CloseDocument { .. }
                | Self::OpenSolution { .. }
                | Self::OpenProjects { .. }
                | Self::WatchedFilesChanged { .. }
        )
    }

    /// Whether sending this can start a design-time build.
    ///
    /// The one predicate the trust gate turns on. It is stated here, on
    /// the request, so that "did we just execute project code" is a
    /// property of the message rather than a convention at the call
    /// site.
    #[must_use]
    pub const fn loads_projects(&self) -> bool {
        matches!(self, Self::OpenSolution { .. } | Self::OpenProjects { .. })
    }

    #[must_use]
    pub fn uri(&self) -> Option<&str> {
        match self {
            Self::Definition { uri, .. }
            | Self::Implementation { uri, .. }
            | Self::References { uri, .. }
            | Self::DocumentSymbol { uri }
            | Self::OpenDocument { uri, .. }
            | Self::ChangeDocument { uri, .. }
            | Self::CloseDocument { uri }
            | Self::OpenSolution { uri } => Some(uri),
            Self::OpenProjects { .. } | Self::WatchedFilesChanged { .. } => None,
        }
    }
}

fn range_value(range: Range) -> Value {
    json!({
        "start": { "line": range.start.line, "character": range.start.character },
        "end": { "line": range.end.line, "character": range.end.character },
    })
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

#[must_use]
pub fn path_to_uri(path: &std::path::Path) -> String {
    crate::python_semantic::protocol::path_to_uri(path)
}

#[must_use]
pub fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    crate::python_semantic::protocol::uri_to_path(uri)
}

/// The directory Roslyn decompiles metadata into.
///
/// A target in a referenced assembly resolves to a file under here, with
/// a content-hash path. The path is useless as identity and the file is
/// not source anyone wrote -- but its header names the assembly, which
/// is real identity. See
/// [`assembly_identity`](crate::csharp_semantic::adapter::assembly_identity).
pub const METADATA_AS_SOURCE: &str = "MetadataAsSource";

/// Whether a URI names decompiled metadata rather than Workspace source.
#[must_use]
pub fn is_metadata_uri(uri: &str) -> bool {
    uri.contains(METADATA_AS_SOURCE)
}

// ---------------------------------------------------------------------
// Answers
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

/// One symbol as `documentSymbol` reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentSymbol {
    pub name: String,
    /// The LSP `SymbolKind` number, kept as the number it is.
    pub kind: i64,
    pub location: Location,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CSharpResponse {
    Locations(Vec<Location>),
    DocumentSymbols(Vec<DocumentSymbol>),
    /// The backend does not implement the method.
    Unsupported(String),
    /// The backend withdrew the request. Ask again, or record a gap;
    /// never treat it as "nothing is there".
    Cancelled(String),
    Delivered,
}

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
pub fn decode(request: &CSharpRequest, result: &Value) -> Result<CSharpResponse, DecodeError> {
    let (name, _) = request.wire();
    match request {
        CSharpRequest::Definition { .. }
        | CSharpRequest::Implementation { .. }
        | CSharpRequest::References { .. } => Ok(CSharpResponse::Locations(match result {
            Value::Null => Vec::new(),
            Value::Array(items) => items
                .iter()
                .map(|item| location(item, name))
                .collect::<Result<Vec<_>, _>>()?,
            single => vec![location(single, name)?],
        })),
        CSharpRequest::DocumentSymbol { .. } => {
            let Some(items) = result.as_array() else {
                if result.is_null() {
                    return Ok(CSharpResponse::DocumentSymbols(Vec::new()));
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
                        location: location(raw, name)?,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CSharpResponse::DocumentSymbols(symbols))
        }
        _ => Ok(CSharpResponse::Delivered),
    }
}

/// The `initialize` parameters this backend is driven with.
///
/// `textDocument.synchronization` is declared here, which is the one
/// place this tier differs from the TypeScript one -- and it is declared
/// because it was measured necessary, not because LSP offers it.
#[must_use]
pub fn initialize_params(root_uri: &str, process_id: u32) -> Value {
    json!({
        "processId": process_id,
        "rootUri": root_uri,
        "capabilities": {
            "general": { "positionEncodings": ["utf-16"] },
            "textDocument": {
                "definition": { "linkSupport": true },
                "implementation": { "linkSupport": true },
                "references": {},
                "documentSymbol": { "hierarchicalDocumentSymbolSupport": false },
                "synchronization": { "dynamicRegistration": true, "didSave": false },
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
    fn a_workspace_is_untrusted_until_something_says_otherwise() {
        assert_eq!(
            ProjectExecutionTrust::default(),
            ProjectExecutionTrust::Untrusted
        );
        assert!(!ProjectExecutionTrust::Untrusted.may_load_projects());
        assert!(ProjectExecutionTrust::Trusted.may_load_projects());
    }

    #[test]
    fn only_a_project_load_request_can_execute_project_code() {
        // The predicate the trust gate turns on. Stated on the request so
        // that "did we just execute project code" is a property of the
        // message rather than a convention at the call site.
        assert!(
            CSharpRequest::OpenSolution {
                uri: "file:///w/x.sln".into()
            }
            .loads_projects()
        );
        assert!(
            CSharpRequest::OpenProjects {
                uris: vec!["file:///w/a.csproj".into()]
            }
            .loads_projects()
        );
        for harmless in [
            CSharpRequest::Definition {
                uri: "file:///w/a.cs".into(),
                position: Position::new(0, 0),
            },
            CSharpRequest::OpenDocument {
                uri: "file:///w/a.cs".into(),
                text: String::new(),
                version: 1,
            },
            CSharpRequest::WatchedFilesChanged {
                changes: Vec::new(),
            },
        ] {
            assert!(!harmless.loads_projects(), "{harmless:?}");
        }
    }

    #[test]
    fn a_patch_inside_the_class_stays_comparable_and_a_minor_does_not() {
        assert_eq!(
            ProtocolCompatibility::classify(TESTED_SERVER_VERSION),
            ProtocolCompatibility::Tested
        );
        // These builds ship daily; pinning the patch would refuse every
        // install but one.
        assert_eq!(
            ProtocolCompatibility::classify("5.4.0-2.26200.1"),
            ProtocolCompatibility::CompatiblePatch
        );
        for version in ["5.5.0-1.1", "6.0.0", "", "next"] {
            assert_eq!(
                ProtocolCompatibility::classify(version),
                ProtocolCompatibility::Unknown,
                "{version} must not be read as 5.4"
            );
        }
        assert_eq!(
            ProtocolCompatibility::Unknown.publication_verdict(),
            BackendCompatibility::Unknown
        );
    }

    #[test]
    fn document_synchronization_is_declared_because_it_was_measured_necessary() {
        // The TypeScript tier declares none. This one must, and the
        // capability declaration is the earliest place it is visible.
        let params = initialize_params("file:///w", 1);
        assert!(
            params["capabilities"]["textDocument"]
                .get("synchronization")
                .is_some()
        );
        assert_eq!(
            params["capabilities"]["textDocument"]["synchronization"]["didSave"], false,
            "there is no editor here to save"
        );
    }

    #[test]
    fn a_changed_document_replaces_the_whole_previous_text() {
        // Full text rather than a delta -- Brainprint has the current
        // bytes and no edit stream, so a synthetic delta would be
        // inventing one -- but expressed as the replacement of
        // everything that was there. The server declares
        // `textDocumentSync.change: 2`, and it does not merely ignore a
        // change with no range: the measured one exits on one, taking
        // the connection with it.
        let request = CSharpRequest::ChangeDocument {
            uri: "file:///w/a.cs".into(),
            text: "class A {}".into(),
            version: 7,
            replaces: Range::new(Position::new(0, 0), Position::new(2, 1)),
        };
        let (name, params) = request.wire();
        assert_eq!(name, method::DID_CHANGE);
        let params = params.expect("params");
        assert_eq!(params["textDocument"]["version"], 7);
        assert_eq!(params["contentChanges"][0]["text"], "class A {}");
        assert_eq!(params["contentChanges"][0]["range"]["start"]["line"], 0);
        assert_eq!(params["contentChanges"][0]["range"]["end"]["line"], 2);
        assert_eq!(params["contentChanges"][0]["range"]["end"]["character"], 1);
        assert!(request.is_notification());
    }

    #[test]
    fn a_shape_this_build_does_not_understand_is_an_error_not_an_empty_answer() {
        let request = CSharpRequest::Definition {
            uri: "file:///w/a.cs".into(),
            position: Position::new(0, 0),
        };
        assert!(decode(&request, &json!([{ "target": "x" }])).is_err());
        let CSharpResponse::Locations(found) =
            decode(&request, &Value::Null).expect("null is an answer")
        else {
            panic!("locations");
        };
        assert!(found.is_empty());
    }

    #[test]
    fn the_measured_partial_type_answer_keeps_every_declaration() {
        // Verbatim shape from the #19 task 12 probe: a reference to a
        // partial type answers with *both* declarations, which is the
        // whole reason `logical_symbol` exists.
        let request = CSharpRequest::Definition {
            uri: "file:///w/src/App/Program.cs".into(),
            position: Position::new(11, 30),
        };
        let measured = json!([
            { "uri": "file:///w/src/Core/Runner.Part1.cs",
              "range": { "start": { "line": 4, "character": 21 },
                         "end": { "line": 4, "character": 27 } } },
            { "uri": "file:///w/src/Core/Runner.Part2.cs",
              "range": { "start": { "line": 4, "character": 21 },
                         "end": { "line": 4, "character": 27 } } },
        ]);
        let CSharpResponse::Locations(found) = decode(&request, &measured).expect("decode") else {
            panic!("locations");
        };
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn decompiled_metadata_is_recognised_wherever_it_lands() {
        assert!(is_metadata_uri(
            "file:///var/folders/T/MetadataAsSource/abc/Console.cs"
        ));
        assert!(!is_metadata_uri("file:///w/src/App/Program.cs"));
    }

    #[test]
    fn the_connection_counts_in_utf16_because_the_server_named_nothing_else() {
        assert_eq!(POSITION_ENCODING, PositionEncoding::Utf16);
    }
}
