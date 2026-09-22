//! Shared fixtures for the Rust backend's always-on tests.
//!
//! No toolchain, no rust-analyzer, no network: a real indexed Cargo
//! workspace plus a scripted backend that answers exactly what #19 task
//! 13 measured the real one answering — including the two answers that
//! shaped the design, a module coming back as a whole file and a `dyn`
//! call resolving to the trait member rather than an impl member.
//!
//! That is also the Level B contract, so the suite that proves the tier
//! degrades is the same suite that proves it works.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process,
    sync::{
        Mutex,
        atomic::{AtomicI64, AtomicU64, Ordering},
    },
};

use brainprint_core::WorkspaceId;

use super::{
    RefreshRequest,
    adapter::RustQueries,
    lifecycle::{self, DocumentVersions, QuiescenceBarrier},
    protocol::{
        Location, POSITION_ENCODING, ProjectExecutionTrust, RustRequest, RustResponse, path_to_uri,
    },
    refresh_resource,
};
use crate::{
    config::WorkspaceConfig,
    lsp::coordinates::{LineMap, Position, Range},
    resource::{Resource, ResourceLanguage, ResourceStore},
    runtime::RequestFailure,
    scan::BaselineScan,
    semantic::{AnalysisContext, ProjectRootIdentity, SemanticBackendKind, ToolchainIdentity},
    semantic_index::SemanticIndex,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// The committed Cargo fixture both the scripted and the real-backend
/// tests read.
#[must_use]
pub fn committed_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("workspaces")
        .join("rust-semantic-spike")
}

pub fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("destination");
    for entry in fs::read_dir(from).expect("read fixture") {
        let entry = entry.expect("entry");
        // Never carry build output -- it holds absolute paths from
        // wherever it was produced, and `OUT_DIR` lives under it -- and
        // never carry the marker a developer's own `cargo test` in the
        // fixture would leave: the trust assertion must be about *this*
        // run.
        let name = entry.file_name();
        if name == "target" || name == "build-rs-ran.marker" {
            continue;
        }
        let target = to.join(&name);
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

/// A Rust AnalysisContext.
#[must_use]
pub fn context() -> AnalysisContext {
    AnalysisContext {
        workspace: WorkspaceId::from_bytes([13; 16]),
        backend: SemanticBackendKind::Rust,
        language: ResourceLanguage::Rust,
        project_root: ProjectRootIdentity::Key("rust-spike".to_owned()),
        toolchain: toolchain(),
    }
}

/// An indexed Cargo workspace on disk.
pub struct Fixture {
    pub base: PathBuf,
    pub root: PathBuf,
}

impl Fixture {
    pub fn create(label: &str) -> Self {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = env::temp_dir().join(format!(
            "brainprint-rustsem-{label}-{}-{sequence}",
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

    /// A location naming bytes `start..end` of `rel`.
    #[must_use]
    pub fn location(&self, rel: &str, start: usize, end: usize) -> Location {
        let text = self.text(rel);
        let map = LineMap::with_encoding(&text, POSITION_ENCODING);
        Location {
            uri: self.uri(rel),
            range: map.range(start, end).expect("range"),
        }
    }

    /// A location naming the declaration named `needle` in `rel`.
    #[must_use]
    pub fn declaration(&self, rel: &str, needle: &str, nth: usize) -> Location {
        let start = self.offset_of(rel, needle, nth);
        self.location(rel, start, start + needle.len())
    }

    /// A location naming a whole *module*, the way the measured server
    /// does: the file, from its first byte past its last line.
    #[must_use]
    pub fn module_location(&self, rel: &str) -> Location {
        let lines = u32::try_from(self.text(rel).lines().count()).unwrap_or(1);
        Location {
            uri: self.uri(rel),
            range: Range::new(Position::new(0, 0), Position::new(lines, 0)),
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
    calls: Vec<RustRequest>,
    /// URIs the client has handed current bytes for, in order.
    opened: Vec<String>,
    /// What the *server* holds, which a duplicate `didOpen` violates.
    held: BTreeSet<String>,
    /// What the client believes the server holds.
    client_held: BTreeMap<String, String>,
    /// Whether the Cargo project has been read.
    project_loaded: bool,
}

/// Answers the questions #19 task 13 measured, and nothing else.
///
/// The trust rule is enforced here rather than assumed: under
/// [`ProjectExecutionTrust::Untrusted`] this backend has no crate graph
/// and therefore answers nothing, which is what the real one did.
pub struct ScriptedBackend {
    state: Mutex<State>,
    definitions: BTreeMap<String, Vec<Location>>,
    implementations: BTreeMap<String, Vec<Location>>,
    failure: Option<RequestFailure>,
    unsupported: bool,
    trust: ProjectExecutionTrust,
    cancel_times: u32,
    cancels_remaining: AtomicI64,
    version: AtomicI64,
    settlings: AtomicU64,
}

impl ScriptedBackend {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            definitions: BTreeMap::new(),
            implementations: BTreeMap::new(),
            failure: None,
            unsupported: false,
            trust: ProjectExecutionTrust::Trusted,
            cancel_times: 0,
            cancels_remaining: AtomicI64::new(0),
            version: AtomicI64::new(0),
            // A freshly started server settles once, before anything is
            // asked of it.
            settlings: AtomicU64::new(1),
        }
    }

    /// A backend for a Workspace nobody trusted for project loading.
    #[must_use]
    pub fn untrusted() -> Self {
        Self {
            trust: ProjectExecutionTrust::Untrusted,
            ..Self::new()
        }
    }

    /// A trusted backend that has already read the Cargo project.
    #[must_use]
    pub fn loaded() -> Self {
        let backend = Self::new();
        backend.state.lock().expect("lock").project_loaded = true;
        backend
    }

    #[must_use]
    pub fn with_definition(mut self, uri: &str, at: Position, locations: Vec<Location>) -> Self {
        self.definitions.insert(position_key(uri, at), locations);
        self
    }

    #[must_use]
    pub fn with_implementation(
        mut self,
        uri: &str,
        at: Position,
        locations: Vec<Location>,
    ) -> Self {
        self.implementations
            .insert(position_key(uri, at), locations);
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

    /// Withdraw the next `times` requests before answering anything.
    #[must_use]
    pub fn cancelling(mut self, times: u32) -> Self {
        self.cancel_times = times;
        self.cancels_remaining = AtomicI64::new(i64::from(times));
        self
    }

    #[must_use]
    pub fn calls(&self) -> Vec<RustRequest> {
        self.state.lock().expect("lock").calls.clone()
    }

    /// Whether the Cargo project was read. The trust assertion every
    /// test that matters makes.
    #[must_use]
    pub fn project_loaded(&self) -> bool {
        self.state.lock().expect("lock").project_loaded
    }

    #[must_use]
    pub fn opened(&self) -> Vec<String> {
        self.state.lock().expect("lock").opened.clone()
    }

    #[must_use]
    pub fn notifications(&self) -> Vec<Vec<super::protocol::WatchedChange>> {
        self.state
            .lock()
            .expect("lock")
            .calls
            .iter()
            .filter_map(|call| match call {
                RustRequest::WatchedFilesChanged { changes } => Some(changes.clone()),
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

impl DocumentVersions for ScriptedBackend {
    fn next_document_version(&self) -> i64 {
        self.version.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn exchange_text(&self, uri: &str, text: &str) -> Option<String> {
        self.state
            .lock()
            .expect("lock")
            .client_held
            .insert(uri.to_owned(), text.to_owned())
    }
}

impl QuiescenceBarrier for ScriptedBackend {
    fn settlings_seen(&self) -> u64 {
        self.settlings.load(Ordering::Acquire)
    }

    fn wait_for_quiescent(&self, seen: u64) -> Result<usize, String> {
        let now = self.settlings.load(Ordering::Acquire);
        if now > seen {
            Ok(usize::try_from(now - seen).unwrap_or(0))
        } else {
            Err("the server never settled".to_owned())
        }
    }
}

impl RustQueries for ScriptedBackend {
    fn call(&self, request: &RustRequest) -> Result<RustResponse, RequestFailure> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        let mut state = self.state.lock().expect("lock");
        state.calls.push(request.clone());
        match request {
            RustRequest::ReloadWorkspace => {
                assert!(
                    self.trust.may_load_projects(),
                    "an untrusted Workspace must never re-read its own manifests"
                );
                state.project_loaded = true;
                // Working, then settled.
                self.settlings.fetch_add(1, Ordering::Release);
                return Ok(RustResponse::Delivered);
            }
            RustRequest::OpenDocument { uri, .. } => {
                assert!(
                    state.held.insert(uri.clone()),
                    "a document may be opened once; re-synchronizing is a didChange"
                );
                state.opened.push(uri.clone());
                return Ok(RustResponse::Delivered);
            }
            RustRequest::ChangeDocument { uri, replaces, .. } => {
                assert!(
                    state.held.contains(uri),
                    "a document must be opened before it can be changed"
                );
                // The server declares incremental sync, so a
                // replacement has to say what it replaces.
                assert!(
                    replaces.end >= replaces.start,
                    "a replacement names a range"
                );
                state.opened.push(uri.clone());
                return Ok(RustResponse::Delivered);
            }
            RustRequest::CloseDocument { .. } | RustRequest::WatchedFilesChanged { .. } => {
                return Ok(RustResponse::Delivered);
            }
            _ => {}
        }
        if self.unsupported {
            return Ok(RustResponse::Unsupported(request.wire().0.to_owned()));
        }
        if self.cancel_times > 0 && self.cancels_remaining.fetch_sub(1, Ordering::Relaxed) > 0 {
            return Ok(RustResponse::Cancelled(request.wire().0.to_owned()));
        }
        let loaded = state.project_loaded;
        drop(state);

        // Measured: with no Cargo project read there is no crate graph,
        // and a crate graph is what every binding needs.
        if !loaded {
            return Ok(RustResponse::Locations(Vec::new()));
        }
        Ok(match request {
            RustRequest::Definition { uri, position } => RustResponse::Locations(
                self.definitions
                    .get(&position_key(uri, *position))
                    .cloned()
                    .unwrap_or_default(),
            ),
            RustRequest::Implementation { uri, position } => RustResponse::Locations(
                self.implementations
                    .get(&position_key(uri, *position))
                    .cloned()
                    .unwrap_or_default(),
            ),
            _ => RustResponse::Locations(Vec::new()),
        })
    }
}

fn position_key(uri: &str, at: Position) -> String {
    format!("{uri}#{}:{}", at.line, at.character)
}

// ---------------------------------------------------------------------
// Driving one refresh
// ---------------------------------------------------------------------

/// Refresh one Resource against a scripted backend.
///
/// # Errors
/// Whatever the refresh returns.
pub fn refresh(
    fixture: &Fixture,
    index: &SemanticIndex,
    queries: &ScriptedBackend,
    rel: &str,
) -> Result<super::RefreshOutcome, super::RustSemanticError> {
    let context = context();
    let packages =
        lifecycle::discover_packages_under(index.connection(), queries.trust, Some(&fixture.root))
            .expect("packages");
    let config = packages.basis();
    let capabilities = super::capability_report(&context, queries.trust);
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
