//! Shared fixtures for the Python backend's always-on tests.
//!
//! Everything here runs without Node, without Pyright, and without a
//! network: a real indexed Workspace plus a scripted backend that
//! answers exactly what #19 task 5 measured the real one answering. The
//! normal test suite therefore never depends on a Pyright someone
//! happens to have installed.

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
    adapter::PythonQueries,
    coordinates::{Position, Range},
    protocol::{Location, PythonRequest, PythonResponse, TypeAnswer, path_to_uri},
};
use crate::{
    config::WorkspaceConfig,
    resource::{Resource, ResourceLanguage, ResourceStore},
    runtime::RequestFailure,
    scan::BaselineScan,
    semantic::{AnalysisContext, ProjectRootIdentity, SemanticBackendKind, ToolchainIdentity},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// The committed Python fixture both the scripted and the real-backend
/// tests read.
#[must_use]
pub fn committed_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("workspaces")
        .join("python-semantic-spike")
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
        backend_version: "1.1.414".to_owned(),
        backend_compatibility_class: super::COMPATIBILITY_CLASS.to_owned(),
        environment_fingerprint: "sha256:test-env".to_owned(),
    }
}

#[must_use]
pub fn context() -> AnalysisContext {
    AnalysisContext {
        workspace: WorkspaceId::from_bytes([5; 16]),
        backend: SemanticBackendKind::Python,
        language: ResourceLanguage::Python,
        project_root: ProjectRootIdentity::Key("pkg".to_owned()),
        toolchain: toolchain(),
    }
}

/// An indexed Python Workspace on disk.
pub struct Fixture {
    pub base: PathBuf,
    pub root: PathBuf,
}

impl Fixture {
    /// Copy the committed Python fixture and index it.
    ///
    /// The same tree the real-backend test drives, so a scripted answer
    /// and a real one are answers about identical source. Copied rather
    /// than used in place, because indexing writes.
    pub fn create(label: &str) -> Self {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = env::temp_dir().join(format!(
            "brainprint-pysem-{label}-{}-{sequence}",
            process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("workspace");
        copy_tree(&committed_fixture(), &root);
        let fixture = Self { base, root };
        BaselineScan::open(&fixture.db_path())
            .expect("index.db")
            .run_initial_scan(
                &fixture.root,
                &WorkspaceConfig::default(),
                "workspace-rev-1",
            )
            .expect("baseline scan");
        fixture
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

    /// The zero-based LSP position of the nth occurrence of `needle`.
    #[must_use]
    pub fn position(&self, rel: &str, needle: &str, nth: usize) -> Position {
        let text = self.text(rel);
        let mut at = None;
        let mut from = 0;
        for _ in 0..=nth {
            let found = text[from..].find(needle).expect("needle") + from;
            at = Some(found);
            from = found + 1;
        }
        let at = at.expect("needle");
        let line = text[..at].matches('\n').count();
        let line_start = text[..at].rfind('\n').map_or(0, |index| index + 1);
        Position::new(
            u32::try_from(line).expect("line fits"),
            u32::try_from(text[line_start..at].encode_utf16().count()).expect("character fits"),
        )
    }

    /// The LSP range of the nth occurrence of `needle`.
    #[must_use]
    pub fn range(&self, rel: &str, needle: &str, nth: usize) -> Range {
        let start = self.position(rel, needle, nth);
        Range::new(
            start,
            Position::new(
                start.line,
                start.character + u32::try_from(needle.encode_utf16().count()).expect("width fits"),
            ),
        )
    }

    /// A location naming the nth occurrence of `needle`, the way the
    /// backend names a definition.
    #[must_use]
    pub fn location(&self, rel: &str, needle: &str, nth: usize) -> Location {
        Location {
            uri: self.uri(rel),
            range: self.range(rel, needle, nth),
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
    snapshot: u64,
    /// While positive, every snapshot-carrying query is refused and the
    /// snapshot moves -- the churn task 5 observed during the backend's
    /// initial analysis.
    stale_budget: u32,
    calls: Vec<PythonRequest>,
}

/// Answers the questions #19 task 5 measured, and nothing else.
pub struct ScriptedBackend {
    state: Mutex<State>,
    definitions: BTreeMap<String, Vec<Location>>,
    imports: BTreeMap<String, Option<String>>,
    types: BTreeMap<String, Option<TypeAnswer>>,
    search_paths: Vec<String>,
    failure: Option<RequestFailure>,
    /// Runs on every call, so a test can move the Workspace underneath
    /// an analysis that is already in flight.
    #[allow(clippy::type_complexity)]
    hook: Option<Box<dyn Fn(&PythonRequest) + Send + Sync>>,
}

impl ScriptedBackend {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                snapshot: 7,
                ..State::default()
            }),
            definitions: BTreeMap::new(),
            imports: BTreeMap::new(),
            types: BTreeMap::new(),
            search_paths: Vec::new(),
            failure: None,
            hook: None,
        }
    }

    #[must_use]
    pub fn with_definition(mut self, uri: &str, at: Position, locations: Vec<Location>) -> Self {
        self.definitions.insert(position_key(uri, at), locations);
        self
    }

    #[must_use]
    pub fn with_import(mut self, uri: &str, dots: u32, parts: &[&str], to: Option<&str>) -> Self {
        self.imports
            .insert(import_key(uri, dots, parts), to.map(str::to_owned));
        self
    }

    #[must_use]
    pub fn with_type(mut self, uri: &str, range: Range, answer: Option<TypeAnswer>) -> Self {
        self.types.insert(range_key(uri, range), answer);
        self
    }

    #[must_use]
    pub fn with_search_paths(mut self, paths: Vec<String>) -> Self {
        self.search_paths = paths;
        self
    }

    /// Refuse the next `count` snapshot-carrying queries as stale,
    /// moving the snapshot each time.
    #[must_use]
    pub fn with_stale_budget(self, count: u32) -> Self {
        self.state.lock().expect("lock").stale_budget = count;
        self
    }

    #[must_use]
    pub fn failing(mut self, failure: RequestFailure) -> Self {
        self.failure = Some(failure);
        self
    }

    /// Run `hook` on every call.
    #[must_use]
    pub fn with_hook(mut self, hook: impl Fn(&PythonRequest) + Send + Sync + 'static) -> Self {
        self.hook = Some(Box::new(hook));
        self
    }

    #[must_use]
    pub fn calls(&self) -> Vec<PythonRequest> {
        self.state.lock().expect("lock").calls.clone()
    }

    #[must_use]
    pub fn snapshots_seen(&self) -> Vec<u64> {
        self.state
            .lock()
            .expect("lock")
            .calls
            .iter()
            .filter_map(PythonRequest::snapshot)
            .collect()
    }
}

impl Default for ScriptedBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl PythonQueries for ScriptedBackend {
    fn call(&self, request: &PythonRequest) -> Result<PythonResponse, RequestFailure> {
        if let Some(hook) = &self.hook {
            hook(request);
        }
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        let mut state = self.state.lock().expect("lock");
        state.calls.push(request.clone());
        if request.snapshot().is_some() && state.stale_budget > 0 {
            state.stale_budget -= 1;
            state.snapshot += 1;
            return Ok(PythonResponse::SnapshotStale);
        }
        let snapshot = state.snapshot;
        drop(state);

        Ok(match request {
            PythonRequest::Snapshot => PythonResponse::Snapshot(snapshot),
            PythonRequest::ProtocolVersion => {
                PythonResponse::ProtocolVersion(super::TESTED_PROTOCOL_VERSION.to_owned())
            }
            PythonRequest::SearchPaths { .. } => {
                PythonResponse::SearchPaths(self.search_paths.clone())
            }
            PythonRequest::ResolveImport {
                source_uri, module, ..
            } => {
                let parts: Vec<&str> = module.name_parts.iter().map(String::as_str).collect();
                PythonResponse::Import(
                    self.imports
                        .get(&import_key(source_uri, module.leading_dots, &parts))
                        .cloned()
                        .flatten(),
                )
            }
            PythonRequest::DeclaredType { uri, range, .. }
            | PythonRequest::ComputedType { uri, range, .. }
            | PythonRequest::ExpectedType { uri, range, .. } => {
                PythonResponse::Type(self.types.get(&range_key(uri, *range)).cloned().flatten())
            }
            PythonRequest::Definition { uri, position }
            | PythonRequest::Declaration { uri, position }
            | PythonRequest::TypeDefinition { uri, position } => PythonResponse::Locations(
                self.definitions
                    .get(&position_key(uri, *position))
                    .cloned()
                    .unwrap_or_default(),
            ),
            PythonRequest::References { .. } => PythonResponse::Locations(Vec::new()),
            PythonRequest::PrepareCallHierarchy { .. } => {
                PythonResponse::CallHierarchyItems(Vec::new())
            }
            PythonRequest::IncomingCalls { .. } => PythonResponse::IncomingCalls(Vec::new()),
            PythonRequest::WatchedFilesChanged { .. } => PythonResponse::Delivered,
        })
    }
}

fn position_key(uri: &str, at: Position) -> String {
    format!("{uri}#{}:{}", at.line, at.character)
}

fn range_key(uri: &str, range: Range) -> String {
    format!(
        "{uri}#{}:{}-{}:{}",
        range.start.line, range.start.character, range.end.line, range.end.character
    )
}

fn import_key(uri: &str, dots: u32, parts: &[&str]) -> String {
    format!("{uri}|{dots}|{}", parts.join("."))
}
