//! Wiring the C# semantic tier to the Workspace lifecycle.
//!
//! ```text
//! filesystem change
//!   → classify: document content, or project structure?
//!   → affected owners, from persisted basis + structural dependents
//!   → withdraw their semantic contributions
//!   → structural replacement (I2/I3)
//!   → content:   Brainprint-owned document sync, per changed document
//!     structure: watched-file batch + project reload + initialization barrier
//!   → refresh affected owners
//!   → publish (task 3) → merge (task 4)
//! ```
//!
//! ## Two change classes, because the measurement found two
//!
//! Editing the body of a file the server already has is *not* the same
//! event as adding a file to a project, and the backend does not treat
//! them the same way.
//!
//! An **existing document's content** changed: a watched-file
//! notification alone left the server answering at the old positions.
//! What worked was handing it Brainprint's current bytes -- see
//! [`synchronize_documents`]. The bytes are the ones the Workspace
//! indexed; there is no editor here and no unsaved buffer to accept.
//!
//! A file's **project membership or configuration** changed: the server
//! has to rebuild the compilation before anything it says about the
//! project is true, and it announces when it is done. So that path waits
//! for [`protocol::method::PROJECT_INITIALIZATION_COMPLETE`], which is a
//! signal rather than a sleep -- see [`reload_projects`].
//!
//! ## Trust is an input, not a setting
//!
//! An untrusted Workspace never loads projects, so the second path
//! cannot run at all and the first answers a narrower set of questions.
//! That difference is in the [`ConfigBasis`], so a publication made
//! untrusted is invalidated the moment trust is granted, rather than
//! silently outliving the answer it was based on.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs,
    path::Path,
};

use brainprint_core::ResourceId;
use rusqlite::{Connection, OptionalExtension, params};

use super::{
    CSharpSemanticError, RefreshOutcome, RefreshRequest, adapter,
    protocol::{ProjectExecutionTrust, WatchedChange, WatchedChangeKind, method, path_to_uri},
};
use crate::{
    db, graph_lifecycle,
    merge::{self, MergeOutcome},
    resource::{Resource, ResourceLanguage},
    semantic::{AnalysisContext, CapabilityReport},
    semantic_index::{
        BACKEND_UNAVAILABLE_CODE, CONFIG_CHANGED_CODE, ConfigBasis, CurrentInputs, SemanticIndex,
        SemanticIndexError, SemanticOwner, SemanticStatus,
    },
};

/// The Level B vocabulary, shared with every other backend.
pub use crate::semantic_lifecycle::{BackendReadiness, SemanticAvailability, availability};

/// The languages this backend answers for.
pub const SERVED_LANGUAGES: [ResourceLanguage; 1] = [ResourceLanguage::CSharp];

// ---------------------------------------------------------------------
// Project structure
// ---------------------------------------------------------------------

/// File extensions that define what a project *is*.
pub const PROJECT_FILE_EXTENSIONS: [&str; 2] = ["csproj", "sln"];

/// Files that change how every project under them compiles.
///
/// MSBuild imports each of these by directory convention, so a change to
/// one can alter the target frameworks, the constants, the analyzers or
/// the package versions of projects that were not themselves touched.
pub const DIRECTORY_CONFIG_NAMES: [&str; 4] = [
    "Directory.Build.props",
    "Directory.Build.targets",
    "Directory.Packages.props",
    "NuGet.config",
];

/// Files that pin what the compilation resolves against.
pub const ENVIRONMENT_NAMES: [&str; 3] = [
    "global.json",
    "packages.lock.json",
    "Directory.Packages.props",
];

/// Which project owns which source file, and what governs them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CSharpProjectConfig {
    /// Every `.csproj`/`.sln` in the Workspace, by path key.
    pub project_files: Vec<Resource>,
    /// The directory-level MSBuild files in force.
    pub directory_config: Vec<Resource>,
    /// The nearest owning project for each active C# Resource.
    ///
    /// This is what a [`LogicalIdentity`](crate::logical_symbol::LogicalIdentity)
    /// names as its compilation world: two `partial class Runner`
    /// declarations in two different projects are two types, and keying
    /// on the project is what keeps them apart.
    pub owning_project: BTreeMap<ResourceId, String>,
    /// What the Workspace is allowed to load.
    pub trust: ProjectExecutionTrust,
}

impl CSharpProjectConfig {
    /// The task 3 [`ConfigBasis`] this configuration contributes.
    ///
    /// The trust mode is in here deliberately. The same Workspace
    /// analysed untrusted and trusted are two different analyses -- one
    /// sees a single document, the other a whole compilation -- so a
    /// publication must not survive the change.
    #[must_use]
    pub fn basis(&self) -> ConfigBasis {
        let mut basis = ConfigBasis::new().with("project_trust", self.trust.as_str());
        for resource in self.project_files.iter().chain(&self.directory_config) {
            basis = basis.with(
                format!("msbuild:{}", resource.path_key),
                content_identity(resource),
            );
        }
        basis
    }

    /// Whether project code would run if a question needed it.
    #[must_use]
    pub const fn loads_projects(&self) -> bool {
        self.trust.may_load_projects()
    }
}

/// Find the MSBuild files and project ownership the Workspace analyses
/// under.
///
/// Ownership is nearest-ancestor: the `.csproj` in the closest directory
/// above a source file. That is what the SDK-style default glob means,
/// and it needs no project file to be parsed or executed to determine --
/// which matters, because under
/// [`ProjectExecutionTrust::Untrusted`] nothing may be executed at all.
///
/// # Errors
/// When the index cannot be read.
pub fn discover_projects(
    connection: &Connection,
    trust: ProjectExecutionTrust,
) -> Result<CSharpProjectConfig, LifecycleError> {
    let mut config = CSharpProjectConfig {
        trust,
        ..CSharpProjectConfig::default()
    };
    for resource in active_resources(connection)? {
        let name = file_name(&resource.path_key);
        if PROJECT_FILE_EXTENSIONS
            .iter()
            .any(|extension| name.ends_with(&format!(".{extension}")))
        {
            config.project_files.push(resource);
        } else if DIRECTORY_CONFIG_NAMES.contains(&name) || ENVIRONMENT_NAMES.contains(&name) {
            config.directory_config.push(resource);
        }
    }

    let project_dirs: Vec<(String, String)> = config
        .project_files
        .iter()
        .filter(|resource| resource.path_key.ends_with(".csproj"))
        .map(|resource| (directory_of(&resource.path_key), resource.path_key.clone()))
        .collect();

    for (resource, path_key) in csharp_sources(connection)? {
        if let Some(project) = nearest_project(&project_dirs, &path_key) {
            config.owning_project.insert(resource, project);
        }
    }
    Ok(config)
}

/// The project whose directory is the longest prefix of this file's.
fn nearest_project(project_dirs: &[(String, String)], path_key: &str) -> Option<String> {
    project_dirs
        .iter()
        .filter(|(directory, _)| under_directory(path_key, directory))
        .max_by_key(|(directory, _)| directory.len())
        .map(|(_, project)| project.clone())
}

fn under_directory(path_key: &str, directory: &str) -> bool {
    directory.is_empty() || path_key.starts_with(&format!("{directory}/"))
}

fn directory_of(path_key: &str) -> String {
    path_key
        .rsplit_once('/')
        .map_or_else(String::new, |(directory, _)| directory.to_owned())
}

fn file_name(path_key: &str) -> &str {
    path_key.rsplit('/').next().unwrap_or(path_key)
}

fn content_identity(resource: &Resource) -> String {
    resource
        .content_hash
        .clone()
        .unwrap_or_else(|| resource.fingerprint.clone())
}

// ---------------------------------------------------------------------
// Environment identity
// ---------------------------------------------------------------------

/// How much of the resolution environment could actually be proven.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentAssurance {
    /// The SDK is pinned and the package graph is locked, so neither can
    /// move without the fingerprint moving with it.
    Proven,
    /// Something can change without the fingerprint moving. The
    /// fingerprint is still deterministic; it is just not evidence.
    Unknown { reason: String },
}

impl EnvironmentAssurance {
    #[must_use]
    pub const fn is_proven(&self) -> bool {
        matches!(self, Self::Proven)
    }
}

/// The environment a C# semantic result depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentIdentity {
    pub fingerprint: String,
    pub assurance: EnvironmentAssurance,
    /// Whether `packages.lock.json` pins the restored package graph.
    pub locked: bool,
}

/// Fingerprint the toolchain and what the compilation resolves against.
///
/// The server build, the trust mode, `global.json` if the SDK is pinned,
/// and every project and directory-level MSBuild file by content
/// identity. No `~/.nuget/packages` is walked, no reference pack is
/// opened, and no absolute NuGet path appears -- a restored package is
/// identified by the lockfile that pinned it, not by where the machine
/// happened to put it.
///
/// Trust is part of the fingerprint because the two modes resolve
/// against different worlds.
///
/// # Errors
/// Never; the signature matches every other backend's so a caller can
/// treat them uniformly.
pub fn environment_identity(
    install: &super::launcher::CSharpInstall,
    config: &CSharpProjectConfig,
) -> Result<EnvironmentIdentity, LifecycleError> {
    let mut fields: Vec<(String, String)> = vec![
        ("language_server".to_owned(), install.server_version.clone()),
        ("runtime".to_owned(), install.runtime_identifier.clone()),
        ("project_trust".to_owned(), config.trust.as_str().to_owned()),
    ];
    let mut unproven: Option<String> = None;

    for resource in config.project_files.iter().chain(&config.directory_config) {
        fields.push((
            format!("msbuild:{}", resource.path_key),
            content_identity(resource),
        ));
    }

    let named = |name: &str| {
        config
            .directory_config
            .iter()
            .any(|resource| file_name(&resource.path_key) == name)
    };
    if !named("global.json") {
        unproven = Some("no global.json pins the SDK this compilation runs on".to_owned());
    }
    let locked = named("packages.lock.json");
    if !locked && unproven.is_none() {
        unproven = Some("no packages.lock.json pins the restored package graph".to_owned());
    }
    if config.project_files.is_empty() && unproven.is_none() {
        unproven = Some("no project file defines a compilation".to_owned());
    }

    let borrowed: Vec<(&str, &str)> = fields
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    Ok(EnvironmentIdentity {
        fingerprint: db::fingerprint("csharp-semantic-env-1", &borrowed),
        assurance: match unproven {
            None => EnvironmentAssurance::Proven,
            Some(reason) => EnvironmentAssurance::Unknown { reason },
        },
        locked,
    })
}

// ---------------------------------------------------------------------
// Module inventory
// ---------------------------------------------------------------------

/// A deterministic identity for the compilation's source set.
///
/// Adding a `.cs` file can change what an untouched file means -- a new
/// `partial` declaration, a new overload, a new extension method in a
/// namespace already imported -- so the file set is an input, and one
/// that has to move whenever a project file does too.
///
/// # Errors
/// When the index cannot be read.
pub fn inventory_fingerprint(connection: &Connection) -> Result<String, rusqlite::Error> {
    let mut statement = connection.prepare(
        "SELECT path_key FROM resource \
         WHERE state = 'ACTIVE' AND (language = 'CSHARP' \
               OR path_key LIKE '%.csproj' OR path_key LIKE '%.sln') \
         ORDER BY path_key",
    )?;
    let keys: Vec<String> = statement
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let fields: Vec<(&str, &str)> = keys.iter().map(|key| ("module", key.as_str())).collect();
    Ok(db::fingerprint("csharp-semantic-inventory-1", &fields))
}

// ---------------------------------------------------------------------
// Change sets
// ---------------------------------------------------------------------

/// What happened to one Resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChangeKind {
    Added,
    Changed,
    Deleted,
    Moved,
}

impl ChangeKind {
    const fn watched(self) -> WatchedChangeKind {
        match self {
            Self::Added => WatchedChangeKind::Created,
            Self::Changed | Self::Moved => WatchedChangeKind::Changed,
            Self::Deleted => WatchedChangeKind::Deleted,
        }
    }

    const fn moves_inventory(self) -> bool {
        matches!(self, Self::Added | Self::Deleted | Self::Moved)
    }
}

/// Which of the two measured synchronization paths a change needs.
///
/// Not a severity ordering and not a heuristic: the backend really does
/// respond to these two events differently, and getting it wrong is a
/// stale answer either way -- an unsynchronized document answers at old
/// positions, an unreloaded project does not know the file exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChangeClass {
    /// An existing document's bytes changed. Hand the backend the
    /// current bytes and query; no reload.
    DocumentContent,
    /// The set of documents in a project, or how a project compiles,
    /// changed. Reload and wait for initialization.
    ProjectStructure,
}

/// One Workspace change, as the lifecycle sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceChange {
    pub resource: ResourceId,
    pub kind: ChangeKind,
    pub path_rel: String,
    pub previous_path_rel: Option<String>,
    pub language: Option<ResourceLanguage>,
}

impl ResourceChange {
    /// A change to a C# source file. Use [`Self::with_language`] for a
    /// project file or a lockfile.
    #[must_use]
    pub fn new(resource: ResourceId, kind: ChangeKind, path_rel: impl Into<String>) -> Self {
        Self {
            resource,
            kind,
            path_rel: path_rel.into(),
            previous_path_rel: None,
            language: Some(ResourceLanguage::CSharp),
        }
    }

    #[must_use]
    pub fn from_previous(mut self, path_rel: impl Into<String>) -> Self {
        self.previous_path_rel = Some(path_rel.into());
        self
    }

    #[must_use]
    pub const fn with_language(mut self, language: Option<ResourceLanguage>) -> Self {
        self.language = language;
        self
    }

    /// Which synchronization path this change needs.
    ///
    /// Editing a file's body is content. Anything that changes *which*
    /// files compile together, or *how* they compile, is structure --
    /// including adding or deleting a `.cs`, because under the SDK-style
    /// default glob that is a project membership change even though no
    /// project file was edited.
    #[must_use]
    pub fn class(&self) -> ChangeClass {
        if touches(self, is_project_structure) || self.kind.moves_inventory() {
            ChangeClass::ProjectStructure
        } else {
            ChangeClass::DocumentContent
        }
    }

    /// Whether this Resource is part of the compilation's source set.
    #[must_use]
    pub fn is_compilation_source(&self) -> bool {
        self.language == Some(ResourceLanguage::CSharp)
    }
}

fn touches(change: &ResourceChange, predicate: impl Fn(&str) -> bool) -> bool {
    predicate(&change.path_rel) || change.previous_path_rel.as_deref().is_some_and(&predicate)
}

fn is_project_structure(path_rel: &str) -> bool {
    let name = file_name(path_rel);
    PROJECT_FILE_EXTENSIONS
        .iter()
        .any(|extension| name.ends_with(&format!(".{extension}")))
        || DIRECTORY_CONFIG_NAMES.contains(&name)
        || ENVIRONMENT_NAMES.contains(&name)
}

fn is_environment_candidate(path_rel: &str) -> bool {
    ENVIRONMENT_NAMES.contains(&file_name(path_rel))
}

/// What one batch of Workspace changes means for one context.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangePlan {
    pub affected: BTreeSet<SemanticOwner>,
    pub inventory_moved: bool,
    pub config_moved: bool,
    pub environment_moved: bool,
    /// Whether any change in the batch needs a project reload.
    pub needs_reload: bool,
}

impl ChangePlan {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.affected.is_empty()
            && !self.inventory_moved
            && !self.config_moved
            && !self.environment_moved
    }
}

/// Which owner contributions a change batch invalidates.
///
/// # Errors
/// When the index cannot be read.
pub fn plan_changes(
    index: &SemanticIndex,
    context: &AnalysisContext,
    changes: &[ResourceChange],
    config: &CSharpProjectConfig,
) -> Result<ChangePlan, LifecycleError> {
    let context_key = context.context_key();
    let mut plan = ChangePlan::default();
    let config_files: BTreeSet<ResourceId> = config
        .project_files
        .iter()
        .chain(&config.directory_config)
        .map(|resource| resource.id)
        .collect();

    for change in changes {
        for owner in index.owners_depending_on(change.resource)? {
            if owner.context_key == context_key {
                plan.affected.insert(owner);
            }
        }
        if change.class() == ChangeClass::ProjectStructure {
            plan.needs_reload = true;
        }
        if change.is_compilation_source() && change.kind.moves_inventory() {
            plan.inventory_moved = true;
        }
        if config_files.contains(&change.resource) || touches(change, is_project_structure) {
            plan.config_moved = true;
        }
        if touches(change, is_environment_candidate) {
            plan.environment_moved = true;
        }
    }

    if plan.inventory_moved || plan.config_moved || plan.environment_moved {
        plan.affected.extend(index.owners_of_context(&context_key)?);
        return Ok(plan);
    }

    let touched: Vec<ResourceId> = changes.iter().map(|change| change.resource).collect();
    let published: BTreeSet<SemanticOwner> =
        index.owners_of_context(&context_key)?.into_iter().collect();
    for dependent in graph_lifecycle::dependents_of(index.connection(), &touched, false)? {
        let owner = SemanticOwner::new(&context_key, dependent);
        if published.contains(&owner) {
            plan.affected.insert(owner);
        }
    }
    Ok(plan)
}

// ---------------------------------------------------------------------
// Synchronization
// ---------------------------------------------------------------------

/// The backend notification for one change batch.
#[must_use]
pub fn watched_changes(workspace_root: &Path, changes: &[ResourceChange]) -> Vec<WatchedChange> {
    let mut by_uri: BTreeMap<String, WatchedChangeKind> = BTreeMap::new();
    for change in changes {
        if let Some(previous) = &change.previous_path_rel
            && previous != &change.path_rel
        {
            by_uri.insert(
                path_to_uri(&workspace_root.join(previous)),
                WatchedChangeKind::Deleted,
            );
        }
        by_uri.insert(
            path_to_uri(&workspace_root.join(&change.path_rel)),
            change.kind.watched(),
        );
    }
    by_uri
        .into_iter()
        .map(|(uri, kind)| WatchedChange { uri, kind })
        .collect()
}

/// Hand the backend Brainprint's current bytes for every changed
/// document.
///
/// This is the measured content path, and the only one that worked: a
/// watched-file notification alone left the server answering at the old
/// positions, and this did not. What is sent is what the Workspace
/// indexed and what is on disk -- see
/// [`super::protocol::DOCUMENT_SYNC_DECISION`]. Nothing unsaved is ever
/// accepted, because there is nothing unsaved to accept.
///
/// Two measured rules decide *how* it is sent, and both are the
/// difference between working and not. A document is opened once: a
/// second `didOpen` made the server answer in-flight requests with
/// `-32800`. And a change carries the range it replaces -- the whole of
/// the previous document -- because the server declares incremental sync
/// and **exits** on a change with no range, taking the connection with
/// it.
///
/// Deleted documents are skipped: there are no current bytes for a file
/// that is gone, and its owner is being withdrawn rather than refreshed.
///
/// # Errors
/// When a document could not be read or the notification could not be
/// delivered.
pub fn synchronize_documents(
    queries: &dyn adapter::CSharpQueries,
    host_versions: &dyn DocumentVersions,
    workspace_root: &Path,
    changes: &[ResourceChange],
) -> Result<usize, LifecycleError> {
    let mut sent = 0;
    for change in changes {
        if change.kind == ChangeKind::Deleted || !change.is_compilation_source() {
            continue;
        }
        let path = workspace_root.join(&change.path_rel);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let uri = path_to_uri(&path);
        let version = host_versions.next_document_version();
        let previous = host_versions.exchange_text(&uri, &text);
        adapter::synchronize_document(queries, uri, text, version, previous.as_deref())
            .map_err(|failure| LifecycleError::Sync(failure.to_string()))?;
        sent += 1;
    }
    Ok(sent)
}

/// Who hands out the monotonic document versions the backend requires.
///
/// One trait method rather than a shared counter, because the version
/// sequence belongs to a *connection*: a restarted backend starts over,
/// and a version that went backwards would be rejected.
pub trait DocumentVersions {
    fn next_document_version(&self) -> i64;

    /// The text this connection last handed the backend for `uri`, and
    /// records `text` as what it now holds.
    ///
    /// `None` means the document is new to this connection, so it is
    /// opened rather than changed -- LSP allows one `didOpen` per
    /// document, and only the connection knows which it has; a restarted
    /// one has none.
    ///
    /// The previous text is needed, not merely a yes/no, because the
    /// measured server declares incremental sync and **exits** on a
    /// change with no range. The replacement therefore has to say what
    /// it replaces, which is everything that was there.
    fn exchange_text(&self, uri: &str, text: &str) -> Option<String>;
}

/// Announce a project-structure change and wait until the backend has
/// rebuilt.
///
/// Three steps, in this order, because each one depends on the last: the
/// filesystem notification tells the server what moved, the reload makes
/// it re-read the project, and the barrier waits for
/// [`method::PROJECT_INITIALIZATION_COMPLETE`] -- the server's
/// own statement that the compilation is ready. Publishing before that
/// signal publishes answers from the previous compilation.
///
/// Refused outright when the Workspace is untrusted: loading a project
/// executes its build logic, and nothing here infers permission to do
/// that. The caller gets [`LifecycleError::Untrusted`] and can degrade,
/// which is the honest outcome -- not a silent load.
///
/// # Errors
/// When the Workspace is untrusted, the notification fails, or the
/// backend does not finish initializing within `timeout`.
pub fn reload_projects(
    queries: &dyn adapter::CSharpQueries,
    barrier: &dyn ProjectLoadBarrier,
    workspace_root: &Path,
    changes: &[ResourceChange],
    config: &CSharpProjectConfig,
) -> Result<usize, LifecycleError> {
    if !config.trust.may_load_projects() {
        return Err(LifecycleError::Untrusted(method::PROJECT_OPEN.to_owned()));
    }
    let projects = projects_to_reload(changes, config);
    if projects.is_empty() {
        return Err(LifecycleError::NoProjectToReload);
    }
    let batch = watched_changes(workspace_root, changes);
    if !batch.is_empty() {
        adapter::notify_watched_files(queries, batch)
            .map_err(|failure| LifecycleError::Sync(failure.to_string()))?;
    }
    let seen = barrier.completions_seen();
    adapter::reopen_projects(
        queries,
        projects
            .iter()
            .map(|path_key| path_to_uri(&workspace_root.join(path_key)))
            .collect(),
    )
    .map_err(|failure| LifecycleError::Sync(failure.to_string()))?;
    barrier
        .wait_for_project_load(seen)
        .map_err(LifecycleError::Sync)
}

/// Which projects a change batch makes stale.
///
/// A changed `.csproj` is obviously one. So is the project that *owns* a
/// added or deleted `.cs`, which is the case the live server found:
/// adding a source file changes a project's Compile item set without
/// touching any project file, and reloading nothing is not a smaller
/// reload -- it is a wait for an announcement that can never come.
///
/// A `.sln` or a directory-level MSBuild file governs every project, so
/// a change to one reloads all of them.
#[must_use]
pub fn projects_to_reload(
    changes: &[ResourceChange],
    config: &CSharpProjectConfig,
) -> BTreeSet<String> {
    let all = || -> BTreeSet<String> {
        config
            .project_files
            .iter()
            .map(|resource| resource.path_key.clone())
            .filter(|path_key| path_key.ends_with(".csproj"))
            .collect()
    };
    let mut projects = BTreeSet::new();
    for change in changes {
        let name = file_name(&change.path_rel);
        if name.ends_with(".sln") || DIRECTORY_CONFIG_NAMES.contains(&name) {
            return all();
        }
        if name.ends_with(".csproj") {
            projects.insert(change.path_rel.replace('\\', "/"));
        } else if let Some(owner) = config.owning_project.get(&change.resource) {
            projects.insert(owner.clone());
        } else if change.is_compilation_source() {
            // A file the index has not placed in a project yet -- which
            // is exactly what an *added* source is. Fall back to the
            // nearest project by path rather than reloading nothing.
            let path_key = change.path_rel.replace('\\', "/");
            let dirs: Vec<(String, String)> = config
                .project_files
                .iter()
                .filter(|resource| resource.path_key.ends_with(".csproj"))
                .map(|resource| (directory_of(&resource.path_key), resource.path_key.clone()))
                .collect();
            if let Some(nearest) = nearest_project(&dirs, &path_key) {
                projects.insert(nearest);
            }
        }
    }
    projects
}

/// Who can wait for the backend's project-initialization signal.
pub trait ProjectLoadBarrier {
    /// How many completions this connection has already announced.
    fn completions_seen(&self) -> u64;
    /// Block until the count moves past `seen`, or time out.
    ///
    /// # Errors
    /// When the signal did not arrive.
    fn wait_for_project_load(&self, seen: u64) -> Result<usize, String>;
}

// ---------------------------------------------------------------------
// Lifecycle steps
// ---------------------------------------------------------------------

/// Why a lifecycle step could not complete.
#[derive(Debug)]
pub enum LifecycleError {
    Index(SemanticIndexError),
    Structural(Box<crate::scan::ScanError>),
    Merge(merge::MergeError),
    Sqlite(rusqlite::Error),
    Refresh(Box<CSharpSemanticError>),
    /// A notification or barrier could not complete, so nothing sent
    /// after it may be treated as current.
    Sync(String),
    /// The step would have executed project build logic in a Workspace
    /// that has not been trusted for it.
    Untrusted(String),
    /// No project in the batch needed reloading, so there would be no
    /// announcement to wait for.
    NoProjectToReload,
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Index(error) => write!(formatter, "semantic index: {error}"),
            Self::Structural(error) => write!(formatter, "structural plan: {error}"),
            Self::Merge(error) => write!(formatter, "semantic merge: {error}"),
            Self::Sqlite(error) => write!(formatter, "index: {error}"),
            Self::Refresh(error) => write!(formatter, "c# refresh: {error}"),
            Self::Sync(detail) => write!(formatter, "backend synchronization: {detail}"),
            Self::Untrusted(method) => write!(
                formatter,
                "{method} loads projects, and this Workspace is not trusted for that"
            ),
            Self::NoProjectToReload => {
                write!(formatter, "no project in this change batch needs reloading")
            }
        }
    }
}

impl Error for LifecycleError {}

impl From<SemanticIndexError> for LifecycleError {
    fn from(error: SemanticIndexError) -> Self {
        Self::Index(error)
    }
}
impl From<crate::scan::ScanError> for LifecycleError {
    fn from(error: crate::scan::ScanError) -> Self {
        Self::Structural(Box::new(error))
    }
}
impl From<merge::MergeError> for LifecycleError {
    fn from(error: merge::MergeError) -> Self {
        Self::Merge(error)
    }
}
impl From<rusqlite::Error> for LifecycleError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
impl From<CSharpSemanticError> for LifecycleError {
    fn from(error: CSharpSemanticError) -> Self {
        Self::Refresh(Box::new(error))
    }
}

/// What one withdrawal pass removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WithdrawReport {
    pub owners: Vec<SemanticOwner>,
    pub gaps_restored: usize,
    pub relations_removed: usize,
    /// Logical symbols left owning no declaration afterwards.
    pub orphaned_groups: usize,
}

/// Withdraw every affected owner's contribution, then mark it dirty.
///
/// Runs *before* structural replacement, for the reason
/// `semantic_evidence.relation_id` has no cascade.
///
/// The extra step here is the group sweep: a withdrawn owner's Symbols
/// are about to be replaced, and a `logical_symbol` whose declarations
/// all belonged to it would otherwise survive as an entity with nothing
/// under it. Membership cascades with the Symbol; the empty group does
/// not, so it is collected.
///
/// # Errors
/// When the index cannot be written.
pub fn withdraw_affected(
    index: &SemanticIndex,
    owners: &BTreeSet<SemanticOwner>,
    error_code: &str,
) -> Result<WithdrawReport, LifecycleError> {
    let connection = index.connection();
    let mut report = WithdrawReport::default();
    for owner in owners {
        let transaction = connection.unchecked_transaction()?;
        let outcome = merge::withdraw(&transaction, &owner.context_key, Some(owner.owner))?;
        crate::logical_symbol::undeclare_resource(&transaction, &owner.context_key, owner.owner)
            .map_err(|error| LifecycleError::Sync(error.to_string()))?;
        transaction.commit()?;
        report.gaps_restored += outcome.gaps_restored;
        report.relations_removed += outcome.relations_removed;
        index.mark_dirty(owner, error_code)?;
        report.owners.push(owner.clone());
    }
    for owner in owners {
        report.orphaned_groups +=
            crate::logical_symbol::collect_orphans(connection, &owner.context_key)
                .map_err(|error| LifecycleError::Sync(error.to_string()))?;
    }
    Ok(report)
}

/// Mark every affected owner unavailable, keeping its last valid
/// publication readable.
///
/// # Errors
/// When the index cannot be written.
pub fn mark_unavailable(
    index: &SemanticIndex,
    owners: &BTreeSet<SemanticOwner>,
    error_code: &str,
) -> Result<(), LifecycleError> {
    for owner in owners {
        index.mark_unavailable(owner, error_code)?;
    }
    Ok(())
}

/// Re-check every owner published for one context against the inputs as
/// they are now.
///
/// # Errors
/// When the index cannot be read.
pub fn revalidate_context(
    index: &SemanticIndex,
    context: &AnalysisContext,
    current: &CurrentInputs,
) -> Result<Vec<(SemanticOwner, SemanticStatus)>, LifecycleError> {
    let mut answers = Vec::new();
    for owner in index.owners_of_context(&context.context_key())? {
        let status = index.revalidate(&owner, current)?;
        answers.push((owner, status));
    }
    Ok(answers)
}

/// The inputs one owner's publication is validated against.
#[must_use]
pub fn current_inputs(
    context: &AnalysisContext,
    config: &ConfigBasis,
    capabilities: &CapabilityReport,
    inventory: &str,
    environment: &EnvironmentIdentity,
) -> CurrentInputs {
    CurrentInputs::new(context, config, capabilities)
        .with_inventory(inventory)
        .with_environment_proven(environment.assurance.is_proven())
}

/// One owner's refresh outcome, or why it did not happen.
#[derive(Debug)]
pub enum OwnerOutcome {
    Refreshed(Box<RefreshOutcome>),
    Failed {
        owner: SemanticOwner,
        error: Box<CSharpSemanticError>,
    },
}

impl OwnerOutcome {
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        matches!(self, Self::Refreshed(_))
    }
}

/// Refresh a set of owners, one at a time, on the shared runtime.
///
/// # Errors
/// When the index cannot be written.
pub fn refresh_owners(
    index: &SemanticIndex,
    queries: &dyn adapter::CSharpQueries,
    request: &mut RefreshRequest<'_>,
    owners: &BTreeSet<SemanticOwner>,
) -> Result<Vec<OwnerOutcome>, LifecycleError> {
    let mut outcomes = Vec::new();
    for owner in owners {
        request.owner = owner.owner;
        match super::refresh_resource(index, queries, request) {
            Ok(outcome) => outcomes.push(OwnerOutcome::Refreshed(Box::new(outcome))),
            Err(error) => {
                index.mark_dirty(owner, BACKEND_UNAVAILABLE_CODE)?;
                outcomes.push(OwnerOutcome::Failed {
                    owner: owner.clone(),
                    error: Box::new(error),
                });
            }
        }
    }
    Ok(outcomes)
}

/// Mark every owner in a context not current because the configuration
/// that governs it changed.
///
/// Granting or revoking trust comes through here, because trust is in
/// the [`ConfigBasis`].
///
/// # Errors
/// When the index cannot be written.
pub fn invalidate_for_config(
    index: &SemanticIndex,
    context: &AnalysisContext,
) -> Result<BTreeSet<SemanticOwner>, LifecycleError> {
    let owners: BTreeSet<SemanticOwner> = index
        .owners_of_context(&context.context_key())?
        .into_iter()
        .collect();
    for owner in &owners {
        index.mark_dirty(owner, CONFIG_CHANGED_CODE)?;
    }
    Ok(owners)
}

/// What a whole merge pass did, summed.
#[must_use]
pub fn total_merged(outcomes: &[OwnerOutcome]) -> MergeOutcome {
    let mut total = MergeOutcome::default();
    for outcome in outcomes {
        if let OwnerOutcome::Refreshed(refreshed) = outcome {
            total.gaps_resolved += refreshed.merged.gaps_resolved;
            total.gaps_restored += refreshed.merged.gaps_restored;
            total.corroborated += refreshed.merged.corroborated;
            total.relations_created += refreshed.merged.relations_created;
            total.relations_removed += refreshed.merged.relations_removed;
            total.conflicts += refreshed.merged.conflicts;
        }
    }
    total
}

// ---------------------------------------------------------------------
// Index reads
// ---------------------------------------------------------------------

type ResourceRow = (Vec<u8>, String, String, String, Option<String>);

fn row_to_resource(row: ResourceRow) -> Option<Resource> {
    let (uid, path_rel, path_key, revision, content_hash) = row;
    let bytes: [u8; 16] = uid.as_slice().try_into().ok()?;
    Some(Resource {
        id: ResourceId::from_bytes(bytes),
        path_rel,
        path_key,
        kind: crate::resource::ResourceKind::File,
        role: crate::resource::ResourceRole::Config,
        language: None,
        size_bytes: 0,
        mtime_ns: 0,
        fingerprint: revision.clone(),
        content_hash,
        state: crate::resource::ResourceState::Active,
        resource_revision: revision,
        generated_kind: None,
        container_resource_id: None,
    })
}

fn active_resources(connection: &Connection) -> Result<Vec<Resource>, LifecycleError> {
    let mut statement = connection.prepare(
        "SELECT uid, path_rel, path_key, resource_revision, content_hash \
         FROM resource WHERE state = 'ACTIVE' ORDER BY path_key",
    )?;
    let rows: Vec<ResourceRow> = statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows.into_iter().filter_map(row_to_resource).collect())
}

fn csharp_sources(connection: &Connection) -> Result<Vec<(ResourceId, String)>, LifecycleError> {
    let mut statement = connection.prepare(
        "SELECT uid, path_key FROM resource \
         WHERE state = 'ACTIVE' AND language = 'CSHARP' ORDER BY path_key",
    )?;
    let rows: Vec<(Vec<u8>, String)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(uid, path_key)| {
            let bytes: [u8; 16] = uid.as_slice().try_into().ok()?;
            Some((ResourceId::from_bytes(bytes), path_key))
        })
        .collect())
}

/// One active Resource by path key, for callers that need a config file
/// by name.
///
/// # Errors
/// When the index cannot be read.
pub fn active_resource_by_path(
    connection: &Connection,
    path_key: &str,
) -> Result<Option<Resource>, LifecycleError> {
    let row: Option<ResourceRow> = connection
        .query_row(
            "SELECT uid, path_rel, path_key, resource_revision, content_hash \
             FROM resource WHERE path_key = ?1 AND state = 'ACTIVE'",
            params![path_key],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    Ok(row.and_then(row_to_resource))
}
