//! Wiring the Python semantic tier to the Workspace lifecycle.
//!
//! Tasks 6 and 7 can answer questions about a file. This is what
//! decides *which* files to ask about, *when*, and in what order --
//! composing the pieces that already exist rather than reimplementing
//! them:
//!
//! ```text
//! filesystem change
//!   → affected owners, from persisted basis
//!   → withdraw their semantic contributions
//!   → structural replacement (I2/I3)
//!   → invalidate owner publications
//!   → tell the backend the filesystem moved
//!   → refresh only the affected owners
//!   → publish (task 3) → merge (task 4)
//! ```
//!
//! ## Why withdrawal comes first
//!
//! `semantic_evidence.relation_id` deliberately has no
//! `ON DELETE CASCADE`. A cascade would silently drop the displaced
//! gaps task 4 restores on withdrawal, so a semantic edge would vanish
//! and leave a *silence* where an honest unresolved site belongs. The
//! price is an ordering obligation: a contribution pointing at
//! relations the structural replacement is about to remove has to be
//! withdrawn before that replacement runs. That is a contract, not an
//! FK error to catch.
//!
//! ## The backend is optional
//!
//! Every entry point here works with no Pyright at all. Missing,
//! incompatible, crashed, in backoff or simply cold: the structural
//! answer stays current, the semantic-required gaps stay visible, and
//! the coverage says so. What never happens is an empty semantic
//! success, and what never happens is a stale semantic-only edge
//! served as current.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
};

use brainprint_core::ResourceId;
use rusqlite::{Connection, OptionalExtension, params};

use super::{
    PythonSemanticError, RefreshOutcome, RefreshRequest, adapter,
    host::PythonSettings,
    protocol::{WatchedChange, WatchedChangeKind, path_to_uri},
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

// ---------------------------------------------------------------------
// Configuration discovery
// ---------------------------------------------------------------------

/// Which file decides what the backend analyzes.
///
/// Pyright's own precedence, not a merge of the two: a
/// `pyrightconfig.json` next to the project root wins outright, and a
/// `pyproject.toml` counts only when it actually carries a
/// `[tool.pyright]` table. Inventing a combined view would make
/// Brainprint's config basis describe a project the launched process
/// does not analyze.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    PyrightConfig,
    PyProject,
    /// Neither file selects the project; the backend uses its defaults
    /// over the project root.
    Defaults,
}

impl ConfigSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PyrightConfig => "PYRIGHTCONFIG_JSON",
            Self::PyProject => "PYPROJECT_TOML",
            Self::Defaults => "DEFAULTS",
        }
    }
}

/// The configuration one AnalysisContext is analyzed under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonProjectConfig {
    pub source: ConfigSource,
    /// The Resource that decides it, when a file does.
    pub resource: Option<Resource>,
}

impl PythonProjectConfig {
    /// The task 3 [`ConfigBasis`] this configuration contributes.
    ///
    /// The selected source is part of it: adding a
    /// `pyrightconfig.json` next to an existing `pyproject.toml`
    /// changes which file governs, and that is a different basis even
    /// if neither file's bytes changed afterwards.
    #[must_use]
    pub fn basis(&self, settings: &PythonSettings) -> ConfigBasis {
        let mut basis = ConfigBasis::new()
            .with("config_source", self.source.as_str())
            .with(
                "config_content",
                self.resource.as_ref().map_or_else(String::new, |resource| {
                    resource
                        .content_hash
                        .clone()
                        .unwrap_or_else(|| resource.fingerprint.clone())
                }),
            )
            .with(
                "config_path",
                self.resource
                    .as_ref()
                    .map_or_else(String::new, |resource| resource.path_key.clone()),
            );
        for (name, value) in settings.basis_inputs() {
            if name != "python_path" && name != "venv_path" {
                basis = basis.with(name, value);
            }
        }
        basis
    }
}

/// The file names Pyright reads, in its own precedence order.
pub const CONFIG_FILE_NAMES: [(&str, ConfigSource); 2] = [
    ("pyrightconfig.json", ConfigSource::PyrightConfig),
    ("pyproject.toml", ConfigSource::PyProject),
];

/// Find the configuration the launched backend will actually use.
///
/// Only Workspace Resources are considered, and only at the project
/// root: no editor settings, no user profile, no ambient state.
pub fn discover_config(
    connection: &Connection,
    workspace_root: &Path,
    project_root_rel: &str,
) -> Result<PythonProjectConfig, LifecycleError> {
    for (name, source) in CONFIG_FILE_NAMES {
        let path_key = if project_root_rel.is_empty() || project_root_rel == "." {
            name.to_owned()
        } else {
            format!("{project_root_rel}/{name}")
        };
        let Some(resource) = active_resource_by_path(connection, &path_key)? else {
            continue;
        };
        if source == ConfigSource::PyProject && !declares_pyright(workspace_root, &resource) {
            // A pyproject.toml without a `[tool.pyright]` table
            // configures something else entirely, and Pyright ignores
            // it. Treating it as the project config would make the
            // basis move on edits that change nothing semantic.
            continue;
        }
        return Ok(PythonProjectConfig {
            source,
            resource: Some(resource),
        });
    }
    Ok(PythonProjectConfig {
        source: ConfigSource::Defaults,
        resource: None,
    })
}

fn declares_pyright(workspace_root: &Path, resource: &Resource) -> bool {
    let Ok(text) = fs::read_to_string(workspace_root.join(&resource.path_rel)) else {
        return false;
    };
    text.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == "[tool.pyright]" || trimmed.starts_with("[tool.pyright.")
    })
}

// ---------------------------------------------------------------------
// Environment identity
// ---------------------------------------------------------------------

/// How much of the resolution environment could actually be proven.
///
/// Two axes, not one. The *fingerprint* is a deterministic identity of
/// what was seen; the assurance says whether what was seen is enough to
/// prove a persisted publication still describes this environment.
/// Collapsing them would force the choice between claiming currentness
/// nobody verified and randomizing the fingerprint so nothing ever
/// stays current -- and a random fingerprint is not an identity.
///
/// Deliberately two states. `Likely` and a confidence score would both
/// be asking a reader to decide what a number means, and the only
/// decision here is binary: may a persisted answer be restored without
/// asking the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentAssurance {
    /// Every resolution-relevant input this observer covers was read,
    /// and each is immutable installation metadata: the same
    /// fingerprint means the same installed environment.
    Proven,
    /// Something in the environment can change without the fingerprint
    /// moving, or could not be read at all. The fingerprint is still
    /// deterministic; it is just not evidence.
    Unknown { reason: String },
}

impl EnvironmentAssurance {
    #[must_use]
    pub const fn is_proven(&self) -> bool {
        matches!(self, Self::Proven)
    }
}

/// The environment a semantic result depends on, as identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentIdentity {
    pub fingerprint: String,
    pub assurance: EnvironmentAssurance,
    /// Diagnostic only: how many installed distributions were seen. Not
    /// a proof state -- an environment with zero readable distributions
    /// and an environment that could not be read are different answers,
    /// and only the assurance distinguishes them.
    pub distributions: usize,
}

/// Reading the pieces of an environment, so a test can fail one.
///
/// Permission bits are not a dependable failure mechanism (CI often
/// runs as root, and Windows ignores the mode), so unreadability has to
/// be injectable to be tested at all.
pub trait EnvironmentFs {
    /// Entry paths directly under `path`, or the reason it failed.
    ///
    /// # Errors
    /// When the directory cannot be listed.
    fn read_dir(&self, path: &Path) -> std::io::Result<Vec<PathBuf>>;
    /// # Errors
    /// When the file cannot be read.
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>>;
    fn is_dir(&self, path: &Path) -> bool;
}

/// The real filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealFs;

impl EnvironmentFs for RealFs {
    fn read_dir(&self, path: &Path) -> std::io::Result<Vec<PathBuf>> {
        let mut paths: Vec<PathBuf> = fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<_>>()?;
        paths.sort();
        Ok(paths)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }
}

/// Fingerprint the interpreter and the environment it resolves imports
/// through.
///
/// Installation *metadata* only -- a distribution's `name-version` and
/// the `RECORD` that pins what its install wrote. No dependency source
/// is opened, hashed, walked or indexed: the goal is a
/// dependency-resolution identity, not a dependency code index.
#[must_use]
pub fn environment_identity(
    workspace_root: &Path,
    settings: &PythonSettings,
) -> EnvironmentIdentity {
    environment_identity_with(workspace_root, settings, &RealFs)
}

/// [`environment_identity`] over an injected filesystem.
#[must_use]
pub fn environment_identity_with(
    workspace_root: &Path,
    settings: &PythonSettings,
    filesystem: &dyn EnvironmentFs,
) -> EnvironmentIdentity {
    let mut fields: Vec<(String, String)> = vec![
        (
            "python_path".to_owned(),
            settings.python_path.clone().unwrap_or_default(),
        ),
        (
            "venv_path".to_owned(),
            settings.venv_path.clone().unwrap_or_default(),
        ),
    ];

    let observation = observe_environment(workspace_root, settings, filesystem);
    for (name, value) in &observation.facts {
        fields.push((name.clone(), value.clone()));
    }

    let borrowed: Vec<(&str, &str)> = fields
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    EnvironmentIdentity {
        fingerprint: db::fingerprint("python-semantic-env-3", &borrowed),
        assurance: match observation.unproven {
            None => EnvironmentAssurance::Proven,
            Some(reason) => EnvironmentAssurance::Unknown { reason },
        },
        distributions: observation.distributions,
    }
}

/// What one pass over `site-packages` established.
#[derive(Default)]
struct Observation {
    /// Deterministic `(name, value)` facts for the fingerprint, in a
    /// stable order.
    facts: Vec<(String, String)>,
    distributions: usize,
    /// The first reason this environment cannot be proven, if any.
    unproven: Option<String>,
}

impl Observation {
    fn unproven(&mut self, reason: impl Into<String>) {
        if self.unproven.is_none() {
            self.unproven = Some(reason.into());
        }
    }
}

fn observe_environment(
    workspace_root: &Path,
    settings: &PythonSettings,
    filesystem: &dyn EnvironmentFs,
) -> Observation {
    let mut observation = Observation::default();

    let Some(venv) = settings
        .venv_path
        .as_ref()
        .map(|venv| resolve_under(workspace_root, venv))
    else {
        observation.unproven("no venv path is configured");
        return observation;
    };
    let Some(directory) = site_packages(&venv, filesystem) else {
        observation.unproven("no site-packages under the configured venv");
        return observation;
    };
    let Ok(entries) = filesystem.read_dir(&directory) else {
        // An unreadable directory is an observation *failure*. Reporting
        // it as an environment with no distributions would turn an I/O
        // error into a proof.
        observation.unproven("site-packages could not be read");
        return observation;
    };

    for entry in entries {
        let name = entry
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(stem) = name.strip_suffix(".dist-info") {
            observation.distributions += 1;
            observe_dist_info(&mut observation, stem, &entry, filesystem);
        } else if let Some(stem) = name.strip_suffix(".egg-info") {
            observation.distributions += 1;
            observation
                .facts
                .push(("egg_info".to_owned(), stem.to_owned()));
            // A legacy egg-info carries no immutable record of what was
            // installed, so its contents can change under an unchanged
            // name.
            observation.unproven(format!("{name} is a legacy install without a RECORD"));
        } else if name.ends_with(".egg-link") {
            observation
                .facts
                .push(("egg_link".to_owned(), name.clone()));
            observation.unproven(format!("{name} links a source checkout"));
        } else if name.ends_with(".pth") {
            observe_pth(&mut observation, &name, &entry, filesystem);
        }
    }
    observation
}

/// A distribution proves itself by its `RECORD`: the installer's own
/// manifest of what it wrote, which changes on a same-version reinstall
/// whose contents differ.
fn observe_dist_info(
    observation: &mut Observation,
    stem: &str,
    directory: &Path,
    filesystem: &dyn EnvironmentFs,
) {
    observation
        .facts
        .push(("distribution".to_owned(), stem.to_owned()));

    match filesystem.read(&directory.join("RECORD")) {
        Ok(bytes) => observation.facts.push((
            format!("record:{stem}"),
            db::fingerprint(
                "python-dist-record-1",
                &[("record", &String::from_utf8_lossy(&bytes))],
            ),
        )),
        Err(_) => observation.unproven(format!("{stem} has no readable RECORD")),
    }

    // PEP 610. An editable or local-path install resolves to a source
    // tree that changes with no installed metadata moving at all.
    if let Ok(bytes) = filesystem.read(&directory.join("direct_url.json")) {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        observation.facts.push((
            format!("direct_url:{stem}"),
            db::fingerprint("python-dist-direct-url-1", &[("direct_url", &text)]),
        ));
        if is_mutable_source(&text) {
            observation.unproven(format!("{stem} is installed from mutable local source"));
        }
    }
}

/// Whether a PEP 610 `direct_url.json` denotes source that can change
/// underneath an unchanged installation.
fn is_mutable_source(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        // Metadata that cannot be understood cannot clear the install.
        return true;
    };
    // A non-editable directory install is a *copy*, and `RECORD`
    // already pins what was copied. An editable one is a pointer at a
    // tree that keeps moving.
    value
        .get("dir_info")
        .and_then(|info| info.get("editable"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// A `.pth` is executed by `site` at interpreter start: a path line adds
/// a directory to `sys.path`, and an `import` line runs code. Neither is
/// covered by any distribution's metadata.
fn observe_pth(
    observation: &mut Observation,
    name: &str,
    path: &Path,
    filesystem: &dyn EnvironmentFs,
) {
    let Ok(bytes) = filesystem.read(path) else {
        observation.unproven(format!("{name} could not be read"));
        return;
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    observation.facts.push((
        format!("pth:{name}"),
        db::fingerprint("python-pth-1", &[("pth", &text)]),
    ));
    if text
        .lines()
        .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
    {
        // Its own bytes are fingerprinted, but what a path line points
        // at is an external tree this observer does not walk.
        observation.unproven(format!("{name} injects import paths"));
    }
}

fn resolve_under(workspace_root: &Path, candidate: &str) -> PathBuf {
    let path = Path::new(candidate);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace_root.join(path)
    }
}

/// `<venv>/lib/pythonX.Y/site-packages`, or the Windows layout.
fn site_packages(venv: &Path, filesystem: &dyn EnvironmentFs) -> Option<PathBuf> {
    let windows = venv.join("Lib").join("site-packages");
    if filesystem.is_dir(&windows) {
        return Some(windows);
    }
    let mut candidates: Vec<PathBuf> = filesystem
        .read_dir(&venv.join("lib"))
        .ok()?
        .into_iter()
        .map(|entry| entry.join("site-packages"))
        .filter(|path| filesystem.is_dir(path))
        .collect();
    candidates.sort();
    candidates.into_iter().next()
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
    /// Identity preserved, locator moved. The Resource is the same one;
    /// its module path may not be.
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

    /// Whether this change can move a module in or out of the project,
    /// which Pyright resolves program-wide.
    const fn moves_inventory(self) -> bool {
        matches!(self, Self::Added | Self::Deleted | Self::Moved)
    }
}

/// One Workspace change, as the lifecycle sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceChange {
    pub resource: ResourceId,
    pub kind: ChangeKind,
    /// The current Workspace-relative path. For a move, the new one.
    pub path_rel: String,
    /// The path the Resource used to be at, for a move or a delete.
    pub previous_path_rel: Option<String>,
    pub language: Option<ResourceLanguage>,
}

impl ResourceChange {
    #[must_use]
    pub fn new(resource: ResourceId, kind: ChangeKind, path_rel: impl Into<String>) -> Self {
        Self {
            resource,
            kind,
            path_rel: path_rel.into(),
            previous_path_rel: None,
            language: Some(ResourceLanguage::Python),
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

    const fn is_python(&self) -> bool {
        matches!(self.language, Some(ResourceLanguage::Python))
    }
}

/// What one batch of Workspace changes means for one context.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangePlan {
    /// The owner contributions that must stop being current.
    pub affected: BTreeSet<SemanticOwner>,
    /// Whether the Python module inventory moved, which can change
    /// import resolution for owners that did not change at all.
    pub inventory_moved: bool,
    /// Whether the selected configuration moved.
    pub config_moved: bool,
}

impl ChangePlan {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.affected.is_empty() && !self.inventory_moved && !self.config_moved
    }
}

/// Which owner contributions a change batch invalidates.
///
/// Driven by the persisted basis, so a Resource nobody read invalidates
/// nobody. An inventory move is the one thing that reaches further:
/// Pyright resolves modules program-wide, so adding or removing one can
/// change what an untouched file's imports mean.
pub fn plan_changes(
    index: &SemanticIndex,
    context: &AnalysisContext,
    changes: &[ResourceChange],
    config: &PythonProjectConfig,
) -> Result<ChangePlan, LifecycleError> {
    let context_key = context.context_key();
    let mut plan = ChangePlan::default();

    for change in changes {
        for owner in index.owners_depending_on(change.resource)? {
            if owner.context_key == context_key {
                plan.affected.insert(owner);
            }
        }
        if change.is_python() && change.kind.moves_inventory() {
            plan.inventory_moved = true;
        }
        if config
            .resource
            .as_ref()
            .is_some_and(|selected| selected.id == change.resource)
            || is_config_candidate(&change.path_rel)
            || change
                .previous_path_rel
                .as_deref()
                .is_some_and(is_config_candidate)
        {
            plan.config_moved = true;
        }
    }

    if plan.inventory_moved || plan.config_moved {
        // Module resolution and project configuration are properties of
        // the whole project, so every owner published under it has to
        // be re-proved rather than assumed.
        plan.affected.extend(index.owners_of_context(&context_key)?);
        return Ok(plan);
    }

    // The structural replacement does not stop at the Resources that
    // changed. Every Resource whose relations point into them is
    // re-resolved against the structure that then exists, and that
    // re-resolution removes canonical edges. A semantic contribution
    // anchored on one of those edges has to be withdrawn first.
    //
    // The basis alone does not find them: a call site I3 already
    // resolved structurally never becomes a semantic dependency, so the
    // callee's Resource is not in the caller's basis -- and the caller
    // is still re-resolved. Because `semantic_evidence.relation_id`
    // deliberately has no cascade, missing one is not a stale row, it
    // is a failed structural publication (#19 task 9).
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

fn is_config_candidate(path_rel: &str) -> bool {
    let name = path_rel.rsplit('/').next().unwrap_or(path_rel);
    CONFIG_FILE_NAMES
        .iter()
        .any(|(candidate, _)| *candidate == name)
}

// ---------------------------------------------------------------------
// Watched-file notification
// ---------------------------------------------------------------------

/// The backend notification for one change batch.
///
/// Deduped by URI and ordered, so one logical Workspace operation
/// produces one deterministic batch however many owners it touches.
/// A move is two events, because on the filesystem it is two.
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
        let uri = path_to_uri(&workspace_root.join(&change.path_rel));
        let kind = change.kind.watched();
        // A file both created and changed in one batch is created once.
        by_uri
            .entry(uri)
            .and_modify(|held| {
                if *held != kind && *held == WatchedChangeKind::Changed {
                    *held = kind;
                }
            })
            .or_insert(kind);
    }
    by_uri
        .into_iter()
        .map(|(uri, kind)| WatchedChange { uri, kind })
        .collect()
}

// ---------------------------------------------------------------------
// Availability
// ---------------------------------------------------------------------

/// The Level B vocabulary, shared with every other backend rather than
/// spelled twice. #19 task 10 needed the same five states for
/// TypeScript/JavaScript, and two copies of "current but the runtime is
/// cold" would be two things to keep in step.
pub use crate::semantic_lifecycle::{BackendReadiness, SemanticAvailability, availability};

// ---------------------------------------------------------------------
// Lifecycle steps
// ---------------------------------------------------------------------

/// Why a lifecycle step could not complete.
#[derive(Debug)]
pub enum LifecycleError {
    Index(SemanticIndexError),
    /// The structural tier could not be asked what a replacement is
    /// about to re-resolve.
    Structural(Box<crate::scan::ScanError>),
    Merge(merge::MergeError),
    Sqlite(rusqlite::Error),
    Refresh(Box<PythonSemanticError>),
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Index(error) => write!(formatter, "semantic index: {error}"),
            Self::Structural(error) => write!(formatter, "structural plan: {error}"),
            Self::Merge(error) => write!(formatter, "semantic merge: {error}"),
            Self::Sqlite(error) => write!(formatter, "index: {error}"),
            Self::Refresh(error) => write!(formatter, "python refresh: {error}"),
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
impl From<PythonSemanticError> for LifecycleError {
    fn from(error: PythonSemanticError) -> Self {
        Self::Refresh(Box::new(error))
    }
}

/// What one withdrawal pass removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WithdrawReport {
    pub owners: Vec<SemanticOwner>,
    pub gaps_restored: usize,
    pub relations_removed: usize,
}

/// Withdraw every affected owner's contribution, then mark it dirty.
///
/// Runs *before* structural replacement. Withdrawal is what restores
/// the displaced structural gaps and garbage-collects semantic-only
/// edges; doing it afterwards would either fail on the relation foreign
/// key or, with a cascade in place, lose the gaps silently.
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
        transaction.commit()?;
        report.gaps_restored += outcome.gaps_restored;
        report.relations_removed += outcome.relations_removed;
        index.mark_dirty(owner, error_code)?;
        report.owners.push(owner.clone());
    }
    Ok(report)
}

/// Mark every affected owner unavailable, keeping its last valid
/// publication readable.
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
/// What a daemon reopen runs. Nothing is launched: a publication whose
/// sources, config, environment, inventory and profile all still hold
/// proves itself from disk, and one whose inputs moved reads DIRTY
/// before any backend exists to ask.
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
///
/// The environment enters twice, and has to: its fingerprint says
/// *which* environment, and its assurance says whether that fingerprint
/// is worth comparing. A persisted publication can only be restored as
/// CURRENT when both hold.
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
    /// The refresh was attempted and failed. The owner keeps its
    /// last-valid publication and is marked, never replaced with an
    /// empty success.
    Failed {
        owner: SemanticOwner,
        error: Box<PythonSemanticError>,
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
/// One owner's failure marks that owner and moves on: a backend
/// hiccup on `a.py` is not a reason to tear down a still-valid
/// publication for `b.py`.
pub fn refresh_owners(
    index: &SemanticIndex,
    queries: &dyn adapter::PythonQueries,
    request: &mut RefreshRequest<'_>,
    owners: &BTreeSet<SemanticOwner>,
) -> Result<Vec<OwnerOutcome>, LifecycleError> {
    let mut outcomes = Vec::new();
    for owner in owners {
        request.owner = owner.owner;
        match super::refresh_resource(index, queries, request) {
            Ok(outcome) => outcomes.push(OwnerOutcome::Refreshed(Box::new(outcome))),
            Err(error) => {
                // The publication that is already there stays readable;
                // it just stops describing itself as current.
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

fn active_resource_by_path(
    connection: &Connection,
    path_key: &str,
) -> Result<Option<Resource>, LifecycleError> {
    /// `(uid, path_rel, revision, content_hash, fingerprint)`.
    type ConfigRow = (Vec<u8>, String, String, Option<String>, String);
    let row: Option<ConfigRow> = connection
        .query_row(
            "SELECT uid, path_rel, resource_revision, content_hash, fingerprint \
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
    let Some((uid, path_rel, revision, content_hash, fingerprint)) = row else {
        return Ok(None);
    };
    let bytes: [u8; 16] = uid
        .as_slice()
        .try_into()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    Ok(Some(Resource {
        id: ResourceId::from_bytes(bytes),
        path_rel,
        path_key: path_key.to_owned(),
        kind: crate::resource::ResourceKind::File,
        role: crate::resource::ResourceRole::Config,
        language: None,
        size_bytes: 0,
        mtime_ns: 0,
        fingerprint,
        content_hash,
        state: crate::resource::ResourceState::Active,
        resource_revision: revision,
        generated_kind: None,
        container_resource_id: None,
    }))
}
