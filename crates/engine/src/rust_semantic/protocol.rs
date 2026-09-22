//! The wire shape of the rust-analyzer boundary, as measured.
//!
//! ```text
//! rust-analyzer --stdio     (the toolchain's own executable, by path)
//!         ↕ JSON-RPC over the shared lsp::jsonrpc client
//! CSharpRequest-shaped typed questions, answered in UTF-16 positions
//! ```
//!
//! Every constant here is a measurement rather than a reading of the
//! documentation; `scripts/rust_semantic_spike/README.md` is the record
//! and `probe.py` reproduces it.
//!
//! ## Three things the measurement settled
//!
//! **The barrier exists, but only if asked for.**
//! [`method::SERVER_STATUS`] is sent only when the client declares
//! `experimental.serverStatusNotification`. With it declared the server
//! announces `quiescent: false` while it works and `quiescent: true`
//! when it settles, including around a reload. That is a deterministic
//! public freshness barrier, so nothing here sleeps.
//!
//! **`cargo.noDeps` is not a safety switch.** It reads like one. With
//! it on, every cross-crate answer in the acceptance fixture came back
//! empty -- a workspace member depending on another member is a
//! dependency. See [`SAFE_CONFIGURATION`].
//!
//! **Dispatch is readable from the answer.** A call on a concrete type
//! resolves to the member inside an `impl`; the same call through
//! `&dyn Trait` resolves to the member inside the `trait`. So the
//! honest classification needs no new vocabulary and no hover parsing.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::lsp::coordinates::{Position, PositionEncoding, Range};

/// The shared trust decision.
///
/// Rust's version of the question is `build.rs` and procedural macros
/// rather than MSBuild targets, and it is the same question.
pub use crate::trust::ProjectExecutionTrust;

/// What loading a Cargo project was measured to execute under
/// [`SAFE_CONFIGURATION`].
///
/// Recorded rather than summarised as "sandboxed", because it is not
/// one. What it says is narrower and checkable: with build scripts,
/// procedural macros and flycheck disabled, the marker `build.rs` in
/// the acceptance fixture did not run and `target/` was not created.
/// `cargo metadata` still runs, which is Cargo's own manifest read.
pub const MEASURED_PROJECT_LOAD_EFFECTS: &str = "runs cargo metadata to read manifests; with buildScripts, procMacro and check \
     disabled it executed no build.rs and created no target/; not a sandbox";

/// The rust-analyzer build #19 task 13 measured.
///
/// It ships inside the toolchain and reports the *toolchain's* version,
/// so backend and compiler identity move together here. That is
/// convenient and not guaranteed -- a project-local rust-analyzer
/// versions independently -- which is why both are fingerprinted.
pub const TESTED_SERVER_VERSION: &str = "1.98.1";

/// What this adapter was written against.
pub const COMPATIBILITY_CLASS: &str = "rust-analyzer:1.98";

/// The server declares `utf-16` and nothing else.
pub const POSITION_ENCODING: PositionEncoding = PositionEncoding::Utf16;

/// The JSON-RPC code for a method the peer does not implement.
pub const METHOD_NOT_FOUND: i64 = -32601;

/// The peer withdrew a request rather than answering it.
pub const REQUEST_CANCELLED: i64 = -32800;

/// Why document synchronization is used at all.
pub const DOCUMENT_SYNC_DECISION: &str = "the server declares textDocumentSync {openClose: true, change: 2}; an incremental \
     change carrying the replaced range moved the answer, so Brainprint sends its own \
     current Resource bytes -- never an editor buffer, of which there is none";

/// The configuration that lets the project load without letting the
/// project run.
///
/// Four switches, and one deliberate omission.
///
/// `cargo.buildScripts.enable`, `procMacro.enable` and `check.enable` /
/// `checkOnSave` are the execution controls, and they were verified by
/// consequence rather than by being sent: a marker `build.rs` that
/// writes a file did not write it, and no `target/` appeared.
///
/// `cargo.noDeps` is **not** set, and that is the finding. It reads
/// like a fourth safety switch and is not one: it decides whether
/// dependencies enter the crate graph, and a workspace member's
/// dependency on a sibling member is a dependency. With it on, all
/// twelve cross-crate probes answered with nothing. The no-network
/// intent it seems to serve belongs to Cargo's own offline mode.
#[must_use]
pub fn safe_configuration() -> Value {
    json!({
        "cargo": {
            // Do not run build.rs.
            "buildScripts": { "enable": false },
            // The default feature set, recorded in the basis rather
            // than guessed at per file.
            "features": [],
            "noDefaultFeatures": false,
            // Deliberately absent: "noDeps".
        },
        // Do not run procedural macros. Anything they would declare is
        // absent, which the capability report states.
        "procMacro": { "enable": false, "attributes": { "enable": false } },
        // No flycheck: nothing is built, on save or otherwise.
        "check": { "enable": false },
        "checkOnSave": false,
        // Brainprint owns the watcher; the server is told what moved.
        "files": { "watcher": "client" },
    })
}

/// The same, as a stable string for the configuration basis.
pub const SAFE_CONFIGURATION: &str =
    "buildScripts=false;procMacro=false;check=false;checkOnSave=false;noDeps=unset";

/// The methods this tier speaks.
pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const INITIALIZED: &str = "initialized";
    pub const SHUTDOWN: &str = "shutdown";
    pub const EXIT: &str = "exit";
    pub const CONFIGURATION: &str = "workspace/configuration";
    pub const REGISTER_CAPABILITY: &str = "client/registerCapability";
    pub const UNREGISTER_CAPABILITY: &str = "client/unregisterCapability";
    pub const WORK_DONE_CREATE: &str = "window/workDoneProgress/create";
    pub const PROGRESS: &str = "$/progress";

    /// The barrier. Sent only when the client declares
    /// `experimental.serverStatusNotification`.
    pub const SERVER_STATUS: &str = "experimental/serverStatus";

    /// rust-analyzer's own reload request. Public, and the only
    /// measured way to make a manifest change take effect.
    pub const RELOAD_WORKSPACE: &str = "rust-analyzer/reloadWorkspace";

    pub const DEFINITION: &str = "textDocument/definition";
    pub const REFERENCES: &str = "textDocument/references";
    pub const IMPLEMENTATION: &str = "textDocument/implementation";
    pub const DOCUMENT_SYMBOL: &str = "textDocument/documentSymbol";
    pub const DID_OPEN: &str = "textDocument/didOpen";
    pub const DID_CHANGE: &str = "textDocument/didChange";
    pub const DID_CLOSE: &str = "textDocument/didClose";
    pub const DID_CHANGE_WATCHED_FILES: &str = "workspace/didChangeWatchedFiles";
}

// ---------------------------------------------------------------------
// Positions and locations
// ---------------------------------------------------------------------

/// One place in one file, as the server names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

impl Location {
    /// Whether this names a whole file rather than a declaration in it.
    ///
    /// How a module answers: `use bp_core::runner` came back as the
    /// entire `runner.rs`, from `0:0` to one past its last line. That
    /// is the module, and the canonical target for it is the Resource.
    #[must_use]
    pub const fn is_whole_file(&self) -> bool {
        self.range.start.line == 0 && self.range.start.character == 0 && self.range.end.line > 0
    }
}

/// One declaration the server reports for a document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentSymbol {
    pub name: String,
    pub kind: i64,
    pub location: Location,
}

/// A filesystem change, as the server is told about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WatchedChangeKind {
    Created,
    Changed,
    Deleted,
}

impl WatchedChangeKind {
    const fn code(self) -> i64 {
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

// ---------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------

/// One typed question, or one statement, this tier makes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RustRequest {
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
    /// The whole document again, at a new version, expressed as the
    /// replacement of everything that was there -- the server declares
    /// incremental sync, so a change says what it replaces.
    ChangeDocument {
        uri: String,
        text: String,
        version: i64,
        replaces: Range,
    },
    CloseDocument {
        uri: String,
    },
    WatchedFilesChanged {
        changes: Vec<WatchedChange>,
    },
    /// Re-read the Cargo manifests. Only ever sent under `Trusted`.
    ReloadWorkspace,
}

impl RustRequest {
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
                    "uri": uri, "languageId": "rust", "version": version, "text": text,
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
            Self::WatchedFilesChanged { changes } => (
                method::DID_CHANGE_WATCHED_FILES,
                Some(json!({
                    "changes": changes
                        .iter()
                        .map(|change| json!({ "uri": change.uri, "type": change.kind.code() }))
                        .collect::<Vec<_>>(),
                })),
            ),
            Self::ReloadWorkspace => (method::RELOAD_WORKSPACE, None),
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
                | Self::WatchedFilesChanged { .. }
        )
    }

    /// Whether sending this can make the server read the Workspace's
    /// own project definition.
    ///
    /// Only the reload. The initial load happens at handshake time and
    /// is gated there, for the same reason: `cargo metadata` reads
    /// manifests the Workspace wrote.
    #[must_use]
    pub const fn loads_projects(&self) -> bool {
        matches!(self, Self::ReloadWorkspace)
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
            | Self::CloseDocument { uri } => Some(uri),
            Self::WatchedFilesChanged { .. } | Self::ReloadWorkspace => None,
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
// Responses
// ---------------------------------------------------------------------

/// One answer, decoded into the shapes this build understands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RustResponse {
    Locations(Vec<Location>),
    DocumentSymbols(Vec<DocumentSymbol>),
    /// The backend does not implement the method.
    Unsupported(String),
    /// The backend withdrew the request. Ask again, or record a gap;
    /// never read it as "nothing is there".
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

/// Read one answer, or say exactly what shape arrived instead.
///
/// # Errors
/// When the payload is not a shape this build understands. An
/// unreadable answer is an error rather than an empty result: the two
/// mean opposite things.
pub fn decode(request: &RustRequest, payload: &Value) -> Result<RustResponse, DecodeError> {
    let (name, _) = request.wire();
    match request {
        RustRequest::Definition { .. }
        | RustRequest::Implementation { .. }
        | RustRequest::References { .. } => Ok(RustResponse::Locations(locations(name, payload)?)),
        RustRequest::DocumentSymbol { .. } => Ok(RustResponse::DocumentSymbols(document_symbols(
            name, payload,
        )?)),
        _ => Ok(RustResponse::Delivered),
    }
}

fn locations(name: &'static str, payload: &Value) -> Result<Vec<Location>, DecodeError> {
    if payload.is_null() {
        return Ok(Vec::new());
    }
    let items = match payload {
        Value::Array(items) => items.clone(),
        single @ Value::Object(_) => vec![single.clone()],
        other => {
            return Err(DecodeError {
                method: name,
                detail: format!("expected a location or a list, got {other}"),
            });
        }
    };
    items
        .iter()
        .map(|item| location(name, item))
        .collect::<Result<Vec<_>, _>>()
}

fn location(name: &'static str, item: &Value) -> Result<Location, DecodeError> {
    // A `LocationLink` names the target differently. Brainprint declares
    // `linkSupport: false`, so this is defensive rather than expected,
    // and it refuses anything it cannot read exactly.
    let (uri, range) = match (item.get("uri"), item.get("targetUri")) {
        (Some(uri), _) => (uri, item.get("range")),
        (None, Some(uri)) => (
            uri,
            item.get("targetSelectionRange").or(item.get("targetRange")),
        ),
        _ => (&Value::Null, None),
    };
    let (Some(uri), Some(range)) = (uri.as_str(), range) else {
        return Err(DecodeError {
            method: name,
            detail: format!("a location without a uri and a range: {item}"),
        });
    };
    Ok(Location {
        uri: uri.to_owned(),
        range: decode_range(name, range)?,
    })
}

fn decode_range(name: &'static str, value: &Value) -> Result<Range, DecodeError> {
    let read = |end: &str, field: &str| -> Option<u32> {
        value
            .get(end)?
            .get(field)?
            .as_u64()
            .and_then(|number| u32::try_from(number).ok())
    };
    match (
        read("start", "line"),
        read("start", "character"),
        read("end", "line"),
        read("end", "character"),
    ) {
        (Some(start_line), Some(start_character), Some(end_line), Some(end_character)) => {
            Ok(Range::new(
                Position::new(start_line, start_character),
                Position::new(end_line, end_character),
            ))
        }
        _ => Err(DecodeError {
            method: name,
            detail: format!("a range this build cannot read: {value}"),
        }),
    }
}

fn document_symbols(
    name: &'static str,
    payload: &Value,
) -> Result<Vec<DocumentSymbol>, DecodeError> {
    if payload.is_null() {
        return Ok(Vec::new());
    }
    let Some(items) = payload.as_array() else {
        return Err(DecodeError {
            method: name,
            detail: format!("expected a list, got {payload}"),
        });
    };
    let mut found = Vec::new();
    for item in items {
        let Some(symbol_name) = item.get("name").and_then(Value::as_str) else {
            continue;
        };
        let kind = item.get("kind").and_then(Value::as_i64).unwrap_or_default();
        let Some(located) = item.get("location") else {
            continue;
        };
        found.push(DocumentSymbol {
            name: symbol_name.to_owned(),
            kind,
            location: location(name, located)?,
        });
    }
    Ok(found)
}

// ---------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------

/// The `initialize` parameters, including the one that turns the
/// barrier on.
///
/// `experimental.serverStatusNotification` is not optional here. Without
/// it the server never sends [`method::SERVER_STATUS`], and there is no
/// deterministic way to know when a load or a reload has settled -- only
/// a sleep, which this tier does not do.
#[must_use]
pub fn initialize_params(root_uri: &str, process_id: u32, workspace_name: &str) -> Value {
    json!({
        "processId": process_id,
        "rootUri": root_uri,
        "workspaceFolders": [{ "uri": root_uri, "name": workspace_name }],
        "initializationOptions": safe_configuration(),
        "capabilities": {
            "general": { "positionEncodings": ["utf-16"] },
            "window": { "workDoneProgress": true },
            "experimental": { "serverStatusNotification": true },
            "workspace": {
                "workspaceFolders": true,
                "configuration": true,
                "didChangeWatchedFiles": { "dynamicRegistration": true },
            },
            "textDocument": {
                // There is no editor here, so nothing is ever saved.
                "synchronization": { "didSave": false, "dynamicRegistration": false },
                "definition": { "linkSupport": false },
                "references": {},
                "implementation": { "linkSupport": false },
                "documentSymbol": { "hierarchicalDocumentSymbolSupport": false },
            },
        },
    })
}

/// Whether a `serverStatus` notification says the server has settled.
#[must_use]
pub fn is_quiescent(params: Option<&Value>) -> bool {
    params
        .and_then(|params| params.get("quiescent"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

// ---------------------------------------------------------------------
// Compatibility
// ---------------------------------------------------------------------

/// Whether this adapter may read a given server's answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolCompatibility {
    /// The measured class.
    Tested,
    /// A different patch of the same minor. Read.
    Compatible,
    /// Something else. Refuse rather than guess.
    Unknown,
}

impl ProtocolCompatibility {
    /// Classify a version string such as `1.98.1 (48a229ce 2026-09-01)`.
    #[must_use]
    pub fn classify(version: &str) -> Self {
        let number = version.split_whitespace().next().unwrap_or(version);
        if number == TESTED_SERVER_VERSION {
            return Self::Tested;
        }
        let minor_of = |value: &str| {
            let mut parts = value.split('.');
            Some((parts.next()?.to_owned(), parts.next()?.to_owned()))
        };
        match (minor_of(number), minor_of(TESTED_SERVER_VERSION)) {
            (Some(found), Some(tested)) if found == tested => Self::Compatible,
            _ => Self::Unknown,
        }
    }

    #[must_use]
    pub const fn usable(self) -> bool {
        matches!(self, Self::Tested | Self::Compatible)
    }

    #[must_use]
    pub const fn publication_verdict(self) -> crate::semantic_index::BackendCompatibility {
        match self {
            Self::Tested | Self::Compatible => {
                crate::semantic_index::BackendCompatibility::Compatible
            }
            Self::Unknown => crate::semantic_index::BackendCompatibility::Rebuild,
        }
    }
}

impl fmt::Display for ProtocolCompatibility {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Tested => "the tested build",
            Self::Compatible => "the tested minor",
            Self::Unknown => "outside the tested class",
        })
    }
}

// ---------------------------------------------------------------------
// URIs
// ---------------------------------------------------------------------

/// A `file://` URI for a path.
#[must_use]
pub fn path_to_uri(path: &std::path::Path) -> String {
    crate::python_semantic::protocol::path_to_uri(path)
}

/// The path a `file://` URI names.
#[must_use]
pub fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    crate::python_semantic::protocol::uri_to_path(uri)
}

/// Whether a URI names something rust-analyzer generated rather than
/// something a person can edit.
///
/// The Rust analogue of the Svelte generated-source gate. Macro
/// expansions and the server's own virtual documents are not Workspace
/// source, and no answer naming one may become a canonical span: an
/// Agent handed that text would be handed something it cannot edit,
/// described as something it can.
#[must_use]
pub fn is_virtual_uri(uri: &str) -> bool {
    !uri.starts_with("file://")
        || uri.contains("/rust-analyzer/")
        || uri.contains("macro-expansion")
        || uri.contains("rust-macro-expand")
}

/// Whether a path lies inside a dependency or toolchain tree.
///
/// Those are read transiently to derive an identity and are never
/// indexed: a registry checkout, a git checkout, the sysroot, and build
/// output. `target/` is here too -- `OUT_DIR` lives under it, and
/// build-script output is not project source.
#[must_use]
pub fn is_dependency_path(path: &std::path::Path) -> bool {
    let text = path.to_string_lossy().replace('\\', "/");
    [
        "/.cargo/registry/",
        "/.cargo/git/",
        "/.rustup/toolchains/",
        "/rustlib/src/rust/",
        "/target/debug/build/",
        "/target/release/build/",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_safe_configuration_disables_execution_and_not_the_crate_graph() {
        let config = safe_configuration();
        assert_eq!(config["cargo"]["buildScripts"]["enable"], false);
        assert_eq!(config["procMacro"]["enable"], false);
        assert_eq!(config["check"]["enable"], false);
        assert_eq!(config["checkOnSave"], false);
        // The finding: `noDeps` reads like a fourth execution control
        // and is not one. Setting it emptied every cross-crate answer.
        assert!(
            config["cargo"].get("noDeps").is_none(),
            "noDeps severs a workspace from its own members: {config}"
        );
    }

    #[test]
    fn the_handshake_asks_for_the_barrier() {
        let params = initialize_params("file:///w", 7, "w");
        assert_eq!(
            params["capabilities"]["experimental"]["serverStatusNotification"], true,
            "without this the server never announces that it has settled"
        );
        assert_eq!(
            params["capabilities"]["textDocument"]["synchronization"]["didSave"], false,
            "there is no editor here to save"
        );
        assert_eq!(
            params["initializationOptions"]["procMacro"]["enable"],
            false
        );
    }

    #[test]
    fn quiescent_is_read_from_the_notification() {
        assert!(is_quiescent(Some(
            &json!({ "health": "ok", "quiescent": true })
        )));
        assert!(!is_quiescent(Some(
            &json!({ "health": "ok", "quiescent": false })
        )));
        assert!(!is_quiescent(None));
        assert!(!is_quiescent(Some(&json!({ "health": "ok" }))));
    }

    #[test]
    fn a_changed_document_replaces_the_whole_previous_text() {
        let request = RustRequest::ChangeDocument {
            uri: "file:///w/a.rs".into(),
            text: "fn a() {}".into(),
            version: 3,
            replaces: Range::new(Position::new(0, 0), Position::new(4, 0)),
        };
        let (name, params) = request.wire();
        assert_eq!(name, method::DID_CHANGE);
        let params = params.expect("params");
        assert_eq!(params["contentChanges"][0]["text"], "fn a() {}");
        assert_eq!(params["contentChanges"][0]["range"]["end"]["line"], 4);
        assert!(request.is_notification());
    }

    #[test]
    fn only_a_reload_reads_the_workspaces_own_project_definition() {
        assert!(RustRequest::ReloadWorkspace.loads_projects());
        for request in [
            RustRequest::Definition {
                uri: "file:///w/a.rs".into(),
                position: Position::new(0, 0),
            },
            RustRequest::DocumentSymbol {
                uri: "file:///w/a.rs".into(),
            },
            RustRequest::OpenDocument {
                uri: "file:///w/a.rs".into(),
                text: String::new(),
                version: 1,
            },
        ] {
            assert!(!request.loads_projects(), "{request:?}");
        }
    }

    #[test]
    fn a_module_answers_as_a_whole_file() {
        let module = Location {
            uri: "file:///w/runner.rs".into(),
            range: Range::new(Position::new(0, 0), Position::new(79, 0)),
        };
        assert!(module.is_whole_file());
        let declaration = Location {
            uri: "file:///w/runner.rs".into(),
            range: Range::new(Position::new(13, 11), Position::new(13, 14)),
        };
        assert!(!declaration.is_whole_file());
    }

    #[test]
    fn generated_and_virtual_locations_are_recognised() {
        for uri in [
            "rust-analyzer://macro-expansion/1",
            "file:///w/target/debug/build/x/out/generated.rs?macro-expansion",
            "untitled:Untitled-1",
        ] {
            assert!(is_virtual_uri(uri), "{uri}");
        }
        assert!(!is_virtual_uri("file:///w/crates/core/src/lib.rs"));
    }

    #[test]
    fn dependency_and_toolchain_trees_are_recognised() {
        for path in [
            "/Users/x/.cargo/registry/src/index.crates.io-1/serde-1.0/src/lib.rs",
            "/Users/x/.cargo/git/checkouts/thing-abc/1234/src/lib.rs",
            "/Users/x/.rustup/toolchains/stable-aarch64-apple-darwin/lib/rustlib/src/rust/library/core/src/option.rs",
            "/w/target/debug/build/bp-core-123/out/generated.rs",
        ] {
            assert!(is_dependency_path(std::path::Path::new(path)), "{path}");
        }
        assert!(!is_dependency_path(std::path::Path::new(
            "/w/crates/core/src/lib.rs"
        )));
    }

    #[test]
    fn a_shape_this_build_does_not_understand_is_an_error_not_an_empty_answer() {
        let request = RustRequest::Definition {
            uri: "file:///w/a.rs".into(),
            position: Position::new(0, 0),
        };
        assert!(decode(&request, &json!([{ "target": "x" }])).is_err());
        let RustResponse::Locations(found) =
            decode(&request, &Value::Null).expect("null is an answer")
        else {
            panic!("locations");
        };
        assert!(found.is_empty());
    }

    #[test]
    fn a_patch_inside_the_class_stays_comparable_and_a_minor_does_not() {
        assert_eq!(
            ProtocolCompatibility::classify("1.98.1 (48a229ce 2026-09-01)"),
            ProtocolCompatibility::Tested
        );
        assert_eq!(
            ProtocolCompatibility::classify("1.98.4"),
            ProtocolCompatibility::Compatible
        );
        assert_eq!(
            ProtocolCompatibility::classify("1.99.0"),
            ProtocolCompatibility::Unknown
        );
        assert!(!ProtocolCompatibility::classify("1.99.0").usable());
    }
}
