//! Shared fixtures for the TypeScript backend's always-on tests.
//!
//! Everything here runs without Node, without TypeScript, and without a
//! network: a real indexed Workspace plus a scripted backend that
//! answers exactly what #19 task 10 measured the real one answering.
//! The normal test suite therefore never depends on a TypeScript
//! anyone happens to have installed -- which is also the Level B
//! contract, checked by the suite that exercises it.

use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use brainprint_core::WorkspaceId;

use super::{
    RefreshRequest,
    adapter::TypeScriptQueries,
    lifecycle,
    protocol::{
        Location, PositionEncodingChoice, TypeScriptRequest, TypeScriptResponse, path_to_uri,
    },
    refresh_resource,
};
use crate::{
    config::WorkspaceConfig,
    lsp::coordinates::{LineMap, Position, PositionEncoding},
    resource::{Resource, ResourceLanguage, ResourceStore},
    runtime::RequestFailure,
    scan::BaselineScan,
    semantic::{AnalysisContext, ProjectRootIdentity, SemanticBackendKind, ToolchainIdentity},
    semantic_index::SemanticIndex,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// The committed TypeScript fixture both the scripted and the
/// real-backend tests read.
#[must_use]
pub fn committed_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("workspaces")
        .join("typescript-semantic-spike")
}

pub fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("destination");
    for entry in fs::read_dir(from).expect("read fixture") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy");
        }
    }
}

/// A toolchain identity fixed by hand, so a context key is stable.
#[must_use]
pub fn toolchain() -> ToolchainIdentity {
    ToolchainIdentity {
        backend_version: super::protocol::TESTED_BACKEND_VERSION.to_owned(),
        backend_compatibility_class: super::COMPATIBILITY_CLASS.to_owned(),
        environment_fingerprint: "sha256:test-env".to_owned(),
    }
}

#[must_use]
pub fn context() -> AnalysisContext {
    AnalysisContext {
        workspace: WorkspaceId::from_bytes([7; 16]),
        backend: SemanticBackendKind::TypeScriptJavaScript,
        language: ResourceLanguage::TypeScript,
        project_root: ProjectRootIdentity::Key("typescript-spike".to_owned()),
        toolchain: toolchain(),
    }
}

/// The encoding the measured 7.0.2 handshake settles on.
#[must_use]
pub fn encoding() -> PositionEncodingChoice {
    PositionEncodingChoice::negotiated(PositionEncoding::Utf8)
}

/// An indexed TypeScript/JavaScript Workspace on disk.
pub struct Fixture {
    pub base: PathBuf,
    pub root: PathBuf,
}

impl Fixture {
    /// Copy the committed fixture and index it.
    ///
    /// The same tree the real-backend test drives, so a scripted answer
    /// and a real one are answers about identical source. Copied rather
    /// than used in place, because indexing writes. No `node_modules`
    /// is linked: these tests never reach a dependency, and a suite
    /// that needed one would not be Level-B-clean.
    pub fn create(label: &str) -> Self {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = env::temp_dir().join(format!(
            "brainprint-tssem-{label}-{}-{sequence}",
            process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("workspace");
        copy_tree(&committed_fixture(), &root);
        let fixture = Self { base, root };
        fixture.rescan("workspace-rev-1");
        fixture
    }

    /// Re-run the baseline scan, which is how a test moves the
    /// Workspace forward without hand-writing index rows.
    pub fn rescan(&self, revision: &str) {
        BaselineScan::open(&self.db_path())
            .expect("index.db")
            .run_initial_scan(&self.root, &WorkspaceConfig::default(), revision)
            .expect("baseline scan");
    }

    /// Write a file into the copied Workspace.
    pub fn write(&self, rel: &str, contents: &str) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(path, contents).expect("fixture file");
    }

    pub fn remove(&self, rel: &str) {
        let _ = fs::remove_file(self.root.join(rel));
    }

    #[must_use]
    pub fn db_path(&self) -> PathBuf {
        self.base.join("data").join("index.db")
    }

    #[must_use]
    pub fn resources(&self) -> Vec<Resource> {
        ResourceStore::open(&self.db_path())
            .expect("index.db")
            .list_active()
            .expect("list")
    }

    #[must_use]
    pub fn resource(&self, rel: &str) -> Resource {
        self.resources()
            .into_iter()
            .find(|resource| resource.path_key == rel)
            .unwrap_or_else(|| panic!("{rel} is indexed"))
    }

    #[must_use]
    pub fn uri(&self, rel: &str) -> String {
        path_to_uri(&self.root.join(rel))
    }

    #[must_use]
    pub fn text(&self, rel: &str) -> String {
        fs::read_to_string(self.root.join(rel)).expect("source")
    }

    /// The position of byte `at` in `rel`, counted the way the measured
    /// connection counts.
    #[must_use]
    pub fn position_of(&self, rel: &str, at: usize) -> Position {
        LineMap::with_encoding(&self.text(rel), PositionEncoding::Utf8)
            .position(at)
            .expect("position")
    }

    /// The position the adapter will ask at for a name occurrence: the
    /// *last* character of the span, because a call site spans the whole
    /// callee and asking at the start of `x.run` answers about `x`.
    #[must_use]
    pub fn last_character(&self, rel: &str, start: usize, end: usize) -> Position {
        LineMap::with_encoding(&self.text(rel), PositionEncoding::Utf8)
            .last_character_position(start, end)
            .expect("position")
    }

    /// A location naming bytes `start..end` of `rel`, the way the
    /// backend names a definition.
    #[must_use]
    pub fn location(&self, rel: &str, start: usize, end: usize) -> Location {
        let text = self.text(rel);
        let map = LineMap::with_encoding(&text, PositionEncoding::Utf8);
        Location {
            uri: self.uri(rel),
            range: map.range(start, end).expect("range"),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

// ---------------------------------------------------------------------
// A scripted backend
// ---------------------------------------------------------------------

#[derive(Default)]
struct State {
    calls: Vec<TypeScriptRequest>,
}

/// Answers the questions #19 task 10 measured, and nothing else.
pub struct ScriptedBackend {
    state: Mutex<State>,
    definitions: BTreeMap<String, Vec<Location>>,
    failure: Option<RequestFailure>,
    /// Answer every definition with `Unsupported`, as a server outside
    /// the compatibility class would.
    unsupported: bool,
}

impl ScriptedBackend {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            definitions: BTreeMap::new(),
            failure: None,
            unsupported: false,
        }
    }

    #[must_use]
    pub fn with_definition(mut self, uri: &str, at: Position, locations: Vec<Location>) -> Self {
        self.definitions.insert(position_key(uri, at), locations);
        self
    }

    #[must_use]
    pub fn failing(mut self, failure: RequestFailure) -> Self {
        self.failure = Some(failure);
        self
    }

    #[must_use]
    pub const fn unsupported(mut self) -> Self {
        self.unsupported = true;
        self
    }

    #[must_use]
    pub fn calls(&self) -> Vec<TypeScriptRequest> {
        self.state.lock().expect("lock").calls.clone()
    }

    /// Every watched-file notification the backend received, in order.
    #[must_use]
    pub fn notifications(&self) -> Vec<Vec<super::protocol::WatchedChange>> {
        self.state
            .lock()
            .expect("lock")
            .calls
            .iter()
            .filter_map(|call| match call {
                TypeScriptRequest::WatchedFilesChanged { changes } => Some(changes.clone()),
                _ => None,
            })
            .collect()
    }
}

impl Default for ScriptedBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeScriptQueries for ScriptedBackend {
    fn call(&self, request: &TypeScriptRequest) -> Result<TypeScriptResponse, RequestFailure> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        self.state.lock().expect("lock").calls.push(request.clone());
        if self.unsupported && !request.is_notification() {
            return Ok(TypeScriptResponse::Unsupported(request.wire().0.to_owned()));
        }
        Ok(match request {
            TypeScriptRequest::Definition { uri, position }
            | TypeScriptRequest::TypeDefinition { uri, position }
            | TypeScriptRequest::Implementation { uri, position } => TypeScriptResponse::Locations(
                self.definitions
                    .get(&position_key(uri, *position))
                    .cloned()
                    .unwrap_or_default(),
            ),
            TypeScriptRequest::References { .. } => TypeScriptResponse::Locations(Vec::new()),
            TypeScriptRequest::Hover { .. } => TypeScriptResponse::Signature(None),
            TypeScriptRequest::SignatureHelp { .. } => TypeScriptResponse::SignatureSet(None),
            TypeScriptRequest::DocumentSymbol { .. } => {
                TypeScriptResponse::DocumentSymbols(Vec::new())
            }
            TypeScriptRequest::PrepareCallHierarchy { .. } => {
                TypeScriptResponse::CallHierarchyItems(Vec::new())
            }
            TypeScriptRequest::IncomingCalls { .. } => {
                TypeScriptResponse::IncomingCalls(Vec::new())
            }
            TypeScriptRequest::OutgoingCalls { .. } => {
                TypeScriptResponse::OutgoingCalls(Vec::new())
            }
            TypeScriptRequest::WatchedFilesChanged { .. } => TypeScriptResponse::Delivered,
        })
    }
}

fn position_key(uri: &str, at: Position) -> String {
    format!("{uri}#{}:{}", at.line, at.character)
}

// ---------------------------------------------------------------------
// Driving one refresh
// ---------------------------------------------------------------------

/// Refresh one owner against a scripted backend.
pub fn refresh(
    fixture: &Fixture,
    index: &SemanticIndex,
    queries: &dyn TypeScriptQueries,
    rel: &str,
) -> Result<super::RefreshOutcome, super::TypeScriptSemanticError> {
    let context = context();
    let config = lifecycle::discover_config(index.connection(), &fixture.root, "")
        .expect("config")
        .basis();
    let capabilities = super::capability_report(&context);
    refresh_resource(
        index,
        queries,
        &RefreshRequest {
            context: &context,
            workspace_root: &fixture.root,
            owner: fixture.resource(rel).id,
            config: &config,
            capabilities: &capabilities,
            encoding: encoding(),
        },
    )
}

/// A backend that answers `s.run` in `consumer.ts` with `location`.
pub fn backend_for(fixture: &Fixture, location: Location) -> ScriptedBackend {
    let text = fixture.text("src/consumer.ts");
    let start = text.find("s.run").expect("call site");
    let at = fixture.last_character("src/consumer.ts", start, start + "s.run".len());
    ScriptedBackend::new().with_definition(&fixture.uri("src/consumer.ts"), at, vec![location])
}

/// Where `Service.run` is declared.
pub fn service_run(fixture: &Fixture) -> Location {
    let text = fixture.text("src/core/service.ts");
    let at = text.find("run(): void").expect("Service.run");
    fixture.location("src/core/service.ts", at, at + 3)
}

/// The scripted backend the merge and lifecycle tests share: one that
/// answers `consumer.ts`'s `s.run()` with `Service.run`.
#[must_use]
pub fn backend_for_service_run(fixture: &Fixture) -> ScriptedBackend {
    backend_for(fixture, service_run(fixture))
}
