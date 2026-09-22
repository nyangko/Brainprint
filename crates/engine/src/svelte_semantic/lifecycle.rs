//! Wiring the Svelte semantic tier to the Workspace lifecycle.
//!
//! ```text
//! filesystem change
//!   → affected owners, from persisted basis + structural dependents
//!   → withdraw their semantic contributions
//!   → structural replacement (I2/I3)
//!   → one watched-file batch tells the backend the filesystem moved
//!   → refresh affected owners
//!   → publish (task 3) → merge (task 4)
//! ```
//!
//! The publication owner is always the original `.svelte` Resource.
//! Nothing generated is an owner, a Resource, or a durable source body.
//!
//! ## What is Svelte's rather than TypeScript's
//!
//! The environment. A Svelte semantic answer depends on four package
//! versions rather than one -- the language server, `svelte2tsx`, the
//! Svelte compiler and the TypeScript the server runs its service on --
//! because between them they decide what the component *language* is and
//! what can be proven about it. A `svelte.config.js` is fingerprinted as
//! an input and never executed; see
//! [`IS_TRUSTED`](super::protocol::IS_TRUSTED).
//!
//! Everything else -- the availability vocabulary, withdrawal,
//! revalidation, the change plan -- is shared, because it was never
//! about a language.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs,
    path::Path,
};

use brainprint_core::ResourceId;
use rusqlite::{Connection, OptionalExtension, params};

use super::{
    RefreshOutcome, RefreshRequest, SvelteSemanticError, adapter,
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

/// The Level B vocabulary, shared with every other backend rather than
/// spelled twice.
pub use crate::semantic_lifecycle::{BackendReadiness, SemanticAvailability, availability};

/// The languages this backend answers for.
///
/// One. A `.ts` file next to a component belongs to the
/// TypeScript/JavaScript backend, and the measured server agrees: asked
/// about a `.ts` document it has not been told about, it errors rather
/// than answering. Two backends, two ownerships, no overlap.
pub const SERVED_LANGUAGES: [ResourceLanguage; 1] = [ResourceLanguage::Svelte];

// ---------------------------------------------------------------------
// Project configuration
// ---------------------------------------------------------------------

/// The Svelte configuration file names, in the order they are looked for.
///
/// Fingerprinted as *inputs*, never executed. The measured server runs
/// one of these on startup unless told the project is untrusted, and
/// Brainprint tells it exactly that -- so the file's bytes still change
/// what a trusted run *would* mean, and a change to them still has to
/// invalidate.
pub const CONFIG_FILE_NAMES: [&str; 4] = [
    "svelte.config.js",
    "svelte.config.mjs",
    "svelte.config.cjs",
    "svelte.config.ts",
];

/// The TypeScript configuration a Svelte project's script blocks are
/// checked under.
pub const TS_CONFIG_FILE_NAMES: [&str; 2] = ["tsconfig.json", "jsconfig.json"];

/// What a component's semantics are configured by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SvelteProjectConfig {
    /// The Svelte config file, when the project has one. Present as an
    /// identity, never as behaviour.
    pub svelte_config: Option<Resource>,
    /// The TypeScript/JavaScript config the server's language service
    /// uses.
    pub ts_config: Option<Resource>,
    /// Whether a config exists that Brainprint is deliberately not
    /// executing.
    pub untrusted_config_present: bool,
}

impl SvelteProjectConfig {
    /// The task 3 [`ConfigBasis`] this configuration contributes.
    ///
    /// Both files by content identity, plus the trust decision itself.
    /// The last one matters: the same project analysed with its config
    /// executed and with it blocked are two different analyses, and a
    /// publication must not survive a change in which one happened.
    #[must_use]
    pub fn basis(&self) -> ConfigBasis {
        ConfigBasis::new()
            .with(
                "svelte_config",
                self.svelte_config
                    .as_ref()
                    .map_or_else(String::new, content_identity),
            )
            .with(
                "svelte_config_path",
                self.svelte_config
                    .as_ref()
                    .map_or_else(String::new, |resource| resource.path_key.clone()),
            )
            .with(
                "ts_config",
                self.ts_config
                    .as_ref()
                    .map_or_else(String::new, content_identity),
            )
            .with(
                "config_trusted",
                if super::protocol::IS_TRUSTED {
                    "true"
                } else {
                    "false"
                },
            )
    }

    /// Whether the project asks for something this tier will not do.
    ///
    /// A config that exists and is not executed is a real, bounded
    /// limitation: a project whose components need a preprocessor gets
    /// narrower coverage rather than silently wrong answers, and this is
    /// what a report says so.
    #[must_use]
    pub const fn has_unexecuted_config(&self) -> bool {
        self.untrusted_config_present
    }
}

/// Find the configuration a Svelte project is analysed under.
///
/// # Errors
/// When the index cannot be read.
pub fn discover_config(
    connection: &Connection,
    project_root_rel: &str,
) -> Result<SvelteProjectConfig, LifecycleError> {
    let mut svelte_config = None;
    for name in CONFIG_FILE_NAMES {
        if let Some(resource) = active_resource_by_path(connection, &under(project_root_rel, name))?
        {
            svelte_config = Some(resource);
            break;
        }
    }
    let mut ts_config = None;
    for name in TS_CONFIG_FILE_NAMES {
        if let Some(resource) = active_resource_by_path(connection, &under(project_root_rel, name))?
        {
            ts_config = Some(resource);
            break;
        }
    }
    Ok(SvelteProjectConfig {
        untrusted_config_present: svelte_config.is_some() && !super::protocol::IS_TRUSTED,
        svelte_config,
        ts_config,
    })
}

fn under(directory: &str, name: &str) -> String {
    if directory.is_empty() || directory == "." {
        name.to_owned()
    } else {
        format!("{directory}/{name}")
    }
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
    /// A manifest and a lockfile were both read, and neither points at a
    /// tree that can change without one of them moving.
    Proven,
    /// Something can change without the fingerprint moving, or could not
    /// be read. The fingerprint is still deterministic; it is just not
    /// evidence.
    Unknown { reason: String },
}

impl EnvironmentAssurance {
    #[must_use]
    pub const fn is_proven(&self) -> bool {
        matches!(self, Self::Proven)
    }
}

/// The environment a Svelte semantic result depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentIdentity {
    pub fingerprint: String,
    pub assurance: EnvironmentAssurance,
    pub lockfile: Option<String>,
}

/// The lockfiles this tier recognises, as opaque current inputs.
pub const LOCKFILES: [&str; 4] = [
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lockb",
];

/// The manifest that declares the dependencies.
pub const MANIFEST: &str = "package.json";

/// Fingerprint the toolchain and the tree it resolves packages through.
///
/// Four tool versions, because all four decide what a component means:
/// the language server, `svelte2tsx`, the Svelte compiler, and the
/// TypeScript the server's language service is. Then the project's
/// manifest and lockfile by content identity. No `node_modules` is
/// walked and nothing inside a dependency is opened.
///
/// # Errors
/// When the index cannot be read.
pub fn environment_identity(
    connection: &Connection,
    workspace_root: &Path,
    project_root_rel: &str,
    install: &super::launcher::SvelteInstall,
) -> Result<EnvironmentIdentity, LifecycleError> {
    let mut fields: Vec<(String, String)> =
        vec![("language_server".to_owned(), install.server_version.clone())];
    for (name, version) in &install.companions {
        fields.push((format!("tool:{name}"), version.clone().unwrap_or_default()));
    }
    let mut unproven: Option<String> = None;

    let manifest = active_resource_by_path(connection, &under(project_root_rel, MANIFEST))?;
    match &manifest {
        Some(resource) => {
            fields.push(("manifest".to_owned(), content_identity(resource)));
            if let Some(reason) = mutable_dependency(workspace_root, resource) {
                unproven = Some(reason);
            }
        }
        None => {
            unproven = Some(format!("no {MANIFEST} under {project_root_rel:?}"));
        }
    }

    let mut lockfile = None;
    for name in LOCKFILES {
        if let Some(resource) = active_resource_by_path(connection, &under(project_root_rel, name))?
        {
            fields.push((format!("lock:{name}"), content_identity(&resource)));
            lockfile = Some(name.to_owned());
            break;
        }
    }
    if lockfile.is_none() && unproven.is_none() {
        unproven = Some("no lockfile pins the installed dependency tree".to_owned());
    }

    let borrowed: Vec<(&str, &str)> = fields
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    Ok(EnvironmentIdentity {
        fingerprint: db::fingerprint("svelte-semantic-env-1", &borrowed),
        assurance: match unproven {
            None => EnvironmentAssurance::Proven,
            Some(reason) => EnvironmentAssurance::Unknown { reason },
        },
        lockfile,
    })
}

fn mutable_dependency(workspace_root: &Path, manifest: &Resource) -> Option<String> {
    let text = fs::read_to_string(workspace_root.join(&manifest.path_rel)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    if value.get("workspaces").is_some() {
        return Some(format!(
            "{MANIFEST} declares workspaces, whose linked packages resolve to mutable source"
        ));
    }
    for section in ["dependencies", "devDependencies", "optionalDependencies"] {
        let Some(entries) = value.get(section).and_then(serde_json::Value::as_object) else {
            continue;
        };
        for (name, requirement) in entries {
            if requirement.as_str().is_some_and(|requirement| {
                ["file:", "link:", "workspace:", "portal:"]
                    .iter()
                    .any(|prefix| requirement.starts_with(prefix))
            }) {
                return Some(format!("{name} resolves to a mutable local tree"));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------
// Module inventory
// ---------------------------------------------------------------------

/// A deterministic identity for the module set a resolution happens
/// against.
///
/// Components *and* the TypeScript around them: `import Child from
/// './lib/Child.svelte'` and `import type { Model } from './lib/model'`
/// are both resolved by the same language service, so adding either kind
/// of file can change what an untouched component's imports mean.
///
/// # Errors
/// When the index cannot be read.
pub fn inventory_fingerprint(connection: &Connection) -> Result<String, rusqlite::Error> {
    let mut statement = connection.prepare(
        "SELECT path_key FROM resource \
         WHERE state = 'ACTIVE' AND language IN ('SVELTE', 'TYPESCRIPT', 'JAVASCRIPT') \
         ORDER BY path_key",
    )?;
    let keys: Vec<String> = statement
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let fields: Vec<(&str, &str)> = keys.iter().map(|key| ("module", key.as_str())).collect();
    Ok(db::fingerprint("svelte-semantic-inventory-1", &fields))
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
    /// A change to a Svelte component. Use [`Self::with_language`] for
    /// anything else -- the `.ts` beside it, a config, a lockfile.
    #[must_use]
    pub fn new(resource: ResourceId, kind: ChangeKind, path_rel: impl Into<String>) -> Self {
        Self {
            resource,
            kind,
            path_rel: path_rel.into(),
            previous_path_rel: None,
            language: Some(ResourceLanguage::Svelte),
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

    /// Whether this Resource is part of the module inventory the Svelte
    /// language service resolves against.
    #[must_use]
    pub fn is_resolvable_module(&self) -> bool {
        matches!(
            self.language,
            Some(
                ResourceLanguage::Svelte
                    | ResourceLanguage::TypeScript
                    | ResourceLanguage::JavaScript
            )
        )
    }
}

/// What one batch of Workspace changes means for one context.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangePlan {
    pub affected: BTreeSet<SemanticOwner>,
    pub inventory_moved: bool,
    pub config_moved: bool,
    pub environment_moved: bool,
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
/// Same three sources as the TypeScript tier, for the same reasons: the
/// persisted semantic basis, the *structural* dependents the replacement
/// will re-resolve, and whole-project invalidation when the
/// configuration, the environment or the module inventory moved.
///
/// # Errors
/// When the index cannot be read.
pub fn plan_changes(
    index: &SemanticIndex,
    context: &AnalysisContext,
    changes: &[ResourceChange],
    config: &SvelteProjectConfig,
) -> Result<ChangePlan, LifecycleError> {
    let context_key = context.context_key();
    let mut plan = ChangePlan::default();
    let config_files: BTreeSet<ResourceId> = config
        .svelte_config
        .iter()
        .chain(config.ts_config.iter())
        .map(|resource| resource.id)
        .collect();

    for change in changes {
        for owner in index.owners_depending_on(change.resource)? {
            if owner.context_key == context_key {
                plan.affected.insert(owner);
            }
        }
        if change.is_resolvable_module() && change.kind.moves_inventory() {
            plan.inventory_moved = true;
        }
        if config_files.contains(&change.resource) || touches(change, is_config_candidate) {
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

fn touches(change: &ResourceChange, predicate: impl Fn(&str) -> bool) -> bool {
    predicate(&change.path_rel) || change.previous_path_rel.as_deref().is_some_and(&predicate)
}

fn file_name(path_rel: &str) -> &str {
    path_rel.rsplit('/').next().unwrap_or(path_rel)
}

fn is_config_candidate(path_rel: &str) -> bool {
    let name = file_name(path_rel);
    CONFIG_FILE_NAMES.contains(&name) || TS_CONFIG_FILE_NAMES.contains(&name)
}

fn is_environment_candidate(path_rel: &str) -> bool {
    let name = file_name(path_rel);
    name == MANIFEST || LOCKFILES.contains(&name)
}

// ---------------------------------------------------------------------
// Watched-file notification
// ---------------------------------------------------------------------

/// The backend notification for one change batch.
///
/// Deduped by URI and ordered, so one logical Workspace operation
/// produces one deterministic batch however many owners it touches.
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

/// Deliver one change batch, and make the components in it answerable.
///
/// Two jobs in one call, and the second is why this exists at all. The
/// measured server refuses a document it has never been told about, so
/// the notification is not only "the filesystem moved" -- it is what
/// brings a component into the server's view in the first place. After
/// this returns, every request on the same connection is answered
/// against the changed filesystem, with no sleep and no settle delay,
/// because JSON-RPC over one ordered stdio connection delivers the
/// notification strictly before anything sent after it.
///
/// # Errors
/// When the notification could not be delivered.
pub fn synchronize(
    queries: &dyn adapter::SvelteQueries,
    workspace_root: &Path,
    changes: &[ResourceChange],
) -> Result<usize, LifecycleError> {
    let batch = watched_changes(workspace_root, changes);
    let count = batch.len();
    if count == 0 {
        return Ok(0);
    }
    adapter::notify_watched_files(queries, batch)
        .map_err(|failure| LifecycleError::Sync(failure.to_string()))?;
    Ok(count)
}

/// Announce every component in the Workspace to a freshly started
/// backend.
///
/// A cold server has seen nothing, and a question about an unannounced
/// component is an error rather than an answer. This is the one-time
/// barrier a restart needs, and it carries no source: a `Changed` event
/// per component, which the server resolves by reading the same bytes
/// Brainprint indexed.
///
/// # Errors
/// When the index cannot be read or the notification cannot be
/// delivered.
pub fn announce_components(
    index: &SemanticIndex,
    queries: &dyn adapter::SvelteQueries,
    workspace_root: &Path,
) -> Result<usize, LifecycleError> {
    let mut statement = index.connection().prepare(
        "SELECT uid, path_rel FROM resource \
         WHERE state = 'ACTIVE' AND language = 'SVELTE' ORDER BY path_key",
    )?;
    let rows: Vec<(Vec<u8>, String)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let changes: Vec<ResourceChange> = rows
        .into_iter()
        .filter_map(|(uid, path_rel)| {
            let bytes: [u8; 16] = uid.as_slice().try_into().ok()?;
            Some(ResourceChange::new(
                ResourceId::from_bytes(bytes),
                ChangeKind::Changed,
                path_rel,
            ))
        })
        .collect();
    synchronize(queries, workspace_root, &changes)
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
    Refresh(Box<SvelteSemanticError>),
    /// The watched-file batch could not be delivered, so no request
    /// after it may be treated as current.
    Sync(String),
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Index(error) => write!(formatter, "semantic index: {error}"),
            Self::Structural(error) => write!(formatter, "structural plan: {error}"),
            Self::Merge(error) => write!(formatter, "semantic merge: {error}"),
            Self::Sqlite(error) => write!(formatter, "index: {error}"),
            Self::Refresh(error) => write!(formatter, "svelte refresh: {error}"),
            Self::Sync(detail) => write!(formatter, "watched-file notification: {detail}"),
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
impl From<SvelteSemanticError> for LifecycleError {
    fn from(error: SvelteSemanticError) -> Self {
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
/// Runs *before* structural replacement, for the reason
/// `semantic_evidence.relation_id` has no cascade.
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
        error: Box<SvelteSemanticError>,
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
    queries: &dyn adapter::SvelteQueries,
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

fn active_resource_by_path(
    connection: &Connection,
    path_key: &str,
) -> Result<Option<Resource>, LifecycleError> {
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
