//! Shared fixtures for the Svelte backend's always-on tests.
//!
//! No Node, no `svelte-language-server`, no network: a real indexed
//! Workspace plus a scripted backend that answers exactly what #19 task
//! 11 measured the real one answering. That is also the Level B
//! contract, so the suite that proves the tier degrades is the same
//! suite that proves it works.

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
    adapter::SvelteQueries,
    lifecycle,
    protocol::{Location, POSITION_ENCODING, SvelteRequest, SvelteResponse, path_to_uri},
    refresh_resource,
};
use crate::{
    config::WorkspaceConfig,
    lsp::coordinates::{LineMap, Position},
    resource::{Resource, ResourceLanguage, ResourceStore},
    runtime::RequestFailure,
    scan::BaselineScan,
    semantic::{AnalysisContext, ProjectRootIdentity, SemanticBackendKind, ToolchainIdentity},
    semantic_index::SemanticIndex,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// The committed Svelte fixture both the scripted and the real-backend
/// tests read.
#[must_use]
pub fn committed_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("workspaces")
        .join("svelte-semantic-spike")
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

#[must_use]
pub fn toolchain() -> ToolchainIdentity {
    ToolchainIdentity {
        backend_version: super::protocol::TESTED_SERVER_VERSION.to_owned(),
        backend_compatibility_class: super::COMPATIBILITY_CLASS.to_owned(),
        environment_fingerprint: "sha256:test-env".to_owned(),
    }
}

/// A Svelte AnalysisContext.
///
/// `SemanticBackendKind::Svelte` and `ResourceLanguage::Svelte`: a
/// distinct runtime identity from the TS/JS one, which is what lets the
/// two backends run different TypeScript versions.
#[must_use]
pub fn context() -> AnalysisContext {
    AnalysisContext {
        workspace: WorkspaceId::from_bytes([11; 16]),
        backend: SemanticBackendKind::Svelte,
        language: ResourceLanguage::Svelte,
        project_root: ProjectRootIdentity::Key("svelte-spike".to_owned()),
        toolchain: toolchain(),
    }
}

/// An indexed Svelte Workspace on disk.
pub struct Fixture {
    pub base: PathBuf,
    pub root: PathBuf,
}

impl Fixture {
    pub fn create(label: &str) -> Self {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = env::temp_dir().join(format!(
            "brainprint-sveltesem-{label}-{}-{sequence}",
            process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("workspace");
        copy_tree(&committed_fixture(), &root);
        let fixture = Self { base, root };
        fixture.rescan("workspace-rev-1");
        fixture
    }

    pub fn rescan(&self, revision: &str) {
        BaselineScan::open(&self.db_path())
            .expect("index.db")
            .run_initial_scan(&self.root, &WorkspaceConfig::default(), revision)
            .expect("baseline scan");
    }

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

    /// The position the adapter will ask at for a name occurrence.
    #[must_use]
    pub fn last_character(&self, rel: &str, start: usize, end: usize) -> Position {
        LineMap::with_encoding(&self.text(rel), POSITION_ENCODING)
            .last_character_position(start, end)
            .expect("position")
    }

    /// A location naming bytes `start..end` of `rel`, the way the
    /// backend names a declaration.
    #[must_use]
    pub fn location(&self, rel: &str, start: usize, end: usize) -> Location {
        let text = self.text(rel);
        let map = LineMap::with_encoding(&text, POSITION_ENCODING);
        Location {
            uri: self.uri(rel),
            range: map.range(start, end).expect("range"),
        }
    }

    /// A location naming a whole *component*, the way the measured
    /// server does: the file, at a degenerate position.
    #[must_use]
    pub fn component_location(&self, rel: &str) -> Location {
        Location {
            uri: self.uri(rel),
            range: crate::lsp::coordinates::Range::new(Position::new(0, 1), Position::new(0, 1)),
        }
    }

    /// The byte offset of the nth occurrence of `needle`.
    #[must_use]
    pub fn offset_of(&self, rel: &str, needle: &str, nth: usize) -> usize {
        let text = self.text(rel);
        text.match_indices(needle)
            .nth(nth)
            .unwrap_or_else(|| panic!("{needle:?} not in {rel}"))
            .0
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
    calls: Vec<SvelteRequest>,
    /// URIs the client has told the server about. The measured server
    /// refuses a document it has never seen, and so does this.
    synchronized: Vec<String>,
}

/// Answers the questions #19 task 11 measured, and nothing else.
pub struct ScriptedBackend {
    state: Mutex<State>,
    definitions: BTreeMap<String, Vec<Location>>,
    failure: Option<RequestFailure>,
    unsupported: bool,
    /// Whether an unsynchronized document errors, as the real one does.
    enforce_sync: bool,
}

impl ScriptedBackend {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            definitions: BTreeMap::new(),
            failure: None,
            unsupported: false,
            enforce_sync: false,
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

    /// Refuse a document nobody synchronized, exactly as the measured
    /// server does.
    #[must_use]
    pub const fn enforcing_synchronization(mut self) -> Self {
        self.enforce_sync = true;
        self
    }

    #[must_use]
    pub fn calls(&self) -> Vec<SvelteRequest> {
        self.state.lock().expect("lock").calls.clone()
    }

    #[must_use]
    pub fn notifications(&self) -> Vec<Vec<super::protocol::WatchedChange>> {
        self.state
            .lock()
            .expect("lock")
            .calls
            .iter()
            .filter_map(|call| match call {
                SvelteRequest::WatchedFilesChanged { changes } => Some(changes.clone()),
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

impl SvelteQueries for ScriptedBackend {
    fn call(&self, request: &SvelteRequest) -> Result<SvelteResponse, RequestFailure> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        let mut state = self.state.lock().expect("lock");
        state.calls.push(request.clone());
        if let SvelteRequest::WatchedFilesChanged { changes } = request {
            for change in changes {
                state.synchronized.push(change.uri.clone());
            }
            return Ok(SvelteResponse::Delivered);
        }
        if self.unsupported {
            return Ok(SvelteResponse::Unsupported(request.wire().0.to_owned()));
        }
        let uri = request.uri().unwrap_or_default().to_owned();
        if self.enforce_sync && !state.synchronized.contains(&uri) {
            return Ok(SvelteResponse::Unsynchronized(uri));
        }
        drop(state);
        Ok(match request {
            SvelteRequest::Definition { uri, position } => SvelteResponse::Locations(
                self.definitions
                    .get(&position_key(uri, *position))
                    .cloned()
                    .unwrap_or_default(),
            ),
            SvelteRequest::WatchedFilesChanged { .. } => SvelteResponse::Delivered,
        })
    }
}

fn position_key(uri: &str, at: Position) -> String {
    format!("{uri}#{}:{}", at.line, at.character)
}

// ---------------------------------------------------------------------
// Driving one refresh
// ---------------------------------------------------------------------

/// Refresh one component against a scripted backend.
///
/// # Errors
/// Whatever the refresh returns.
pub fn refresh(
    fixture: &Fixture,
    index: &SemanticIndex,
    queries: &dyn SvelteQueries,
    rel: &str,
) -> Result<super::RefreshOutcome, super::SvelteSemanticError> {
    let context = context();
    let config = lifecycle::discover_config(index.connection(), "")
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
        },
    )
}

/// The scripted backend the merge and lifecycle tests share.
///
/// It answers the two questions that define the Svelte tier: a template
/// identifier reaching its script declaration, and a component tag
/// reaching the component's own `.svelte` Resource.
#[must_use]
pub fn backend_for_parent(fixture: &Fixture) -> ScriptedBackend {
    let increment_use = fixture.offset_of("src/Parent.svelte", "increment}", 0);
    let increment_declaration = fixture.offset_of("src/Parent.svelte", "increment(): void", 0);
    let child_tag = fixture.offset_of("src/Parent.svelte", "<Child ", 0) + 1;

    ScriptedBackend::new()
        .with_definition(
            &fixture.uri("src/Parent.svelte"),
            fixture.last_character(
                "src/Parent.svelte",
                increment_use,
                increment_use + "increment".len(),
            ),
            vec![fixture.location(
                "src/Parent.svelte",
                increment_declaration,
                increment_declaration + "increment".len(),
            )],
        )
        .with_definition(
            &fixture.uri("src/Parent.svelte"),
            fixture.last_character("src/Parent.svelte", child_tag, child_tag + "Child".len()),
            vec![fixture.component_location("src/lib/Child.svelte")],
        )
}
