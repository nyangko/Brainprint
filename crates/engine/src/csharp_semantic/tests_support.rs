//! Shared fixtures for the C# backend's always-on tests.
//!
//! No .NET SDK, no `Microsoft.CodeAnalysis.LanguageServer`, no network:
//! a real indexed Workspace plus a scripted backend that answers exactly
//! what #19 task 12 measured the real one answering -- including the two
//! answers that shaped the design, a partial type reporting both of its
//! declarations and a framework call landing in decompiled metadata.
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
    adapter::CSharpQueries,
    lifecycle::{self, DocumentVersions, ProjectLoadBarrier},
    protocol::{
        CSharpRequest, CSharpResponse, Location, POSITION_ENCODING, ProjectExecutionTrust,
        path_to_uri,
    },
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

/// The committed C# fixture both the scripted and the real-backend tests
/// read.
#[must_use]
pub fn committed_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("workspaces")
        .join("csharp-semantic-spike")
}

pub fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("destination");
    for entry in fs::read_dir(from).expect("read fixture") {
        let entry = entry.expect("entry");
        let name = entry.file_name();
        // Never copy a previous build's output: it holds absolute paths
        // from wherever it was produced.
        if name == "bin" || name == "obj" {
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

/// A C# AnalysisContext.
#[must_use]
pub fn context() -> AnalysisContext {
    AnalysisContext {
        workspace: WorkspaceId::from_bytes([12; 16]),
        backend: SemanticBackendKind::CSharp,
        language: ResourceLanguage::CSharp,
        project_root: ProjectRootIdentity::Key("csharp-spike".to_owned()),
        toolchain: toolchain(),
    }
}

/// An indexed C# Workspace on disk.
pub struct Fixture {
    pub base: PathBuf,
    pub root: PathBuf,
}

impl Fixture {
    pub fn create(label: &str) -> Self {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = env::temp_dir().join(format!(
            "brainprint-csharpsem-{label}-{}-{sequence}",
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
// Decompiled metadata
// ---------------------------------------------------------------------

/// Write a decompiled metadata file in the measured shape.
///
/// The header the real server writes, reproduced down to the parts that
/// matter: a `#region` whose label is **localized**, an assembly
/// identity after it, and a machine-local DLL path on the next line. The
/// label here is the Korean one the spike actually measured, which is
/// exactly why [`assembly_identity`](super::assembly_identity) must not
/// look for the word "assembly".
pub fn write_metadata(directory: &Path, assembly: &str, type_name: &str, label: &str) -> PathBuf {
    fs::create_dir_all(directory).expect("metadata directory");
    let path = directory.join(format!("{type_name}.cs"));
    fs::write(
        &path,
        format!(
            "#region {label} {assembly}\n\
             // /usr/local/share/dotnet/packs/Microsoft.NETCore.App.Ref/10.0.0/ref/net10.0/{assembly_name}.dll\n\
             // Decompiled with ICSharpCode.Decompiler\n\
             #endregion\n\n\
             namespace System;\n\npublic static class {type_name} {{ }}\n",
            assembly_name = assembly.split(',').next().unwrap_or(assembly).trim(),
        ),
    )
    .expect("metadata file");
    path
}

// ---------------------------------------------------------------------
// A scripted backend
// ---------------------------------------------------------------------

#[derive(Default)]
struct State {
    calls: Vec<CSharpRequest>,
    /// URIs the client has handed current bytes for, in order.
    opened: Vec<String>,
    /// What the *server* holds, which is what a duplicate `didOpen`
    /// would violate. Kept apart from the client's own bookkeeping so
    /// the two can disagree -- which is the bug this catches.
    held: BTreeSet<String>,
    /// What the client believes the server holds, by URI.
    client_held: BTreeMap<String, String>,
    /// Whether a project set has been loaded.
    projects_loaded: bool,
}

/// Answers the questions #19 task 12 measured, and nothing else.
///
/// The trust rule is enforced here rather than assumed: under
/// [`ProjectExecutionTrust::Untrusted`] this backend answers only what
/// the real one answered with no project loaded -- a definition inside
/// the document that was asked about -- and refuses to load projects at
/// all. A test that forgets to grant trust therefore fails the way
/// production would, with gaps.
pub struct ScriptedBackend {
    state: Mutex<State>,
    definitions: BTreeMap<String, Vec<Location>>,
    failure: Option<RequestFailure>,
    unsupported: bool,
    trust: ProjectExecutionTrust,
    /// How many times each site is withdrawn before it is answered.
    ///
    /// The live server does this: a definition request in flight while
    /// the document is being re-analysed comes back `-32800`.
    cancel_times: u32,
    /// How many withdrawals are left to hand out.
    cancels_remaining: AtomicI64,
    /// A monotonic version counter, standing in for the host's.
    version: AtomicI64,
    /// How many project initializations have been announced.
    completions: AtomicU64,
}

impl ScriptedBackend {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            definitions: BTreeMap::new(),
            failure: None,
            unsupported: false,
            trust: ProjectExecutionTrust::Trusted,
            cancel_times: 0,
            cancels_remaining: AtomicI64::new(0),
            version: AtomicI64::new(0),
            completions: AtomicU64::new(0),
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

    /// Withdraw the next `times` requests before answering anything.
    #[must_use]
    pub fn cancelling(mut self, times: u32) -> Self {
        self.cancel_times = times;
        self.cancels_remaining = AtomicI64::new(i64::from(times));
        self
    }

    #[must_use]
    pub fn calls(&self) -> Vec<CSharpRequest> {
        self.state.lock().expect("lock").calls.clone()
    }

    /// Whether any project was loaded. The trust assertion every test
    /// that matters makes.
    #[must_use]
    pub fn projects_loaded(&self) -> bool {
        self.state.lock().expect("lock").projects_loaded
    }

    /// The documents the client handed current bytes for, in order.
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
                CSharpRequest::WatchedFilesChanged { changes } => Some(changes.clone()),
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

impl ProjectLoadBarrier for ScriptedBackend {
    fn completions_seen(&self) -> u64 {
        self.completions.load(Ordering::Acquire)
    }

    fn wait_for_project_load(&self, seen: u64) -> Result<usize, String> {
        if !self.state.lock().expect("lock").projects_loaded {
            return Err("no project load was requested".to_owned());
        }
        // The real barrier blocks until the server announces; the
        // scripted one announces at load time, so this only checks that
        // the caller waited *after* asking.
        let now = self.completions.load(Ordering::Acquire);
        if now > seen {
            Ok(usize::try_from(now - seen).unwrap_or(0))
        } else {
            Err("initialization was never announced".to_owned())
        }
    }
}

impl CSharpQueries for ScriptedBackend {
    fn call(&self, request: &CSharpRequest) -> Result<CSharpResponse, RequestFailure> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        let mut state = self.state.lock().expect("lock");
        state.calls.push(request.clone());
        match request {
            CSharpRequest::OpenSolution { .. } | CSharpRequest::OpenProjects { .. } => {
                assert!(
                    self.trust.may_load_projects(),
                    "an untrusted Workspace must never reach a project load"
                );
                state.projects_loaded = true;
                self.completions.fetch_add(1, Ordering::Release);
                return Ok(CSharpResponse::Delivered);
            }
            CSharpRequest::OpenDocument { uri, .. } => {
                assert!(
                    state.held.insert(uri.clone()),
                    "a document may be opened once; re-synchronizing is a didChange"
                );
                state.opened.push(uri.clone());
                return Ok(CSharpResponse::Delivered);
            }
            CSharpRequest::ChangeDocument { uri, replaces, .. } => {
                assert!(
                    state.held.contains(uri),
                    "a document must be opened before it can be changed"
                );
                // The measured server declares incremental sync and
                // exits on a change with no range, so a replacement has
                // to say what it replaces.
                assert!(
                    replaces.end >= replaces.start,
                    "a replacement names a range"
                );
                state.opened.push(uri.clone());
                return Ok(CSharpResponse::Delivered);
            }
            CSharpRequest::CloseDocument { .. } | CSharpRequest::WatchedFilesChanged { .. } => {
                return Ok(CSharpResponse::Delivered);
            }
            _ => {}
        }
        if self.unsupported {
            return Ok(CSharpResponse::Unsupported(request.wire().0.to_owned()));
        }
        if self.cancel_times > 0 && self.cancels_remaining.fetch_sub(1, Ordering::Relaxed) > 0 {
            return Ok(CSharpResponse::Cancelled(request.wire().0.to_owned()));
        }
        let projects_loaded = state.projects_loaded;
        drop(state);

        Ok(match request {
            CSharpRequest::Definition { uri, position } => {
                let answers = self
                    .definitions
                    .get(&position_key(uri, *position))
                    .cloned()
                    .unwrap_or_default();
                // Measured: with no project loaded, only a target inside
                // the same document is found.
                CSharpResponse::Locations(if projects_loaded {
                    answers
                } else {
                    answers
                        .into_iter()
                        .filter(|location| &location.uri == uri)
                        .collect()
                })
            }
            _ => CSharpResponse::Locations(Vec::new()),
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
) -> Result<super::RefreshOutcome, super::CSharpSemanticError> {
    let context = context();
    let projects =
        lifecycle::discover_projects(index.connection(), queries.trust).expect("projects");
    let config = projects.basis();
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
            projects: &projects,
        },
    )
}
