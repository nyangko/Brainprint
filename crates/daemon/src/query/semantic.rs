//! Lazy Level-A semantic enrichment of one Workspace (#39).
//!
//! Connects the already-implemented I4 pieces -- backend install
//! location, launcher, `SemanticRuntimeSupervisor`, per-language
//! `refresh_owners`, `SemanticIndex` publication and merge -- to the
//! daemon's Workspace runtime. Owned by that Workspace's single worker
//! thread (`runtime.rs`), so every client of the Workspace shares one
//! supervisor, and a different worktree (a different Workspace) has its
//! own.
//!
//! Nothing here decides truth. Demand comes only from canonical gap
//! evidence the structural tier already persisted (a `REQUIRES_SEMANTICS`
//! reason, a candidate set, or a semantic scope that is not current), for
//! the exact target the query selected. Everything else -- publication
//! revalidation, merge, restart budget, backoff, idle unload -- is the
//! existing I4 machinery. `CoreQuerySurface` is never given a backend: the
//! original operation simply runs after the enrichment, on the same
//! surface, and reports whatever gaps remain.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use brainprint_core::{ResourceId, WorkspaceId};
use brainprint_engine::{
    config::{
        ExecutableLocator, InstallRootLocator, NodeBackendLocator, SemanticBackendLocators,
        WorkspaceConfig, load_global_config,
    },
    merge,
    paths::GlobalPaths,
    projection::ProjectionTarget,
    python_semantic::{self as python, PyrightInstall, PythonLauncher, PythonSettings},
    query::Currentness,
    query_surface::{
        CoreQuerySurface, QueryContext, RelationDirection, RelationsRequest, TargetResolution,
    },
    resolution::{Resolution, Support},
    resource::{ResourceLanguage, ResourceStore},
    runtime::{RequestOptions, RuntimePolicy, SemanticRuntimeSupervisor},
    semantic::{AnalysisContext, AnalysisContextBinding, ProjectRootIdentity, SemanticBackendKind},
    semantic_index::{SemanticIndex, SemanticOwner, SemanticState},
    svelte_semantic::{self as svelte, SvelteInstall, SvelteLauncher},
    typescript_semantic::{self as typescript, TypeScriptInstall, TypeScriptLauncher},
};

/// Per-request backend timeout: the value the I4 real-backend acceptance
/// uses (a tuning value, not a contract).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Snapshot-invalidation retries per owner refresh, as I4 acceptance uses.
const PYTHON_BATCH_ATTEMPTS: u32 = 12;

/// `semantic_publication.error_code` for owners withdrawn ahead of a
/// structural publication (the code the I4 lifecycle benchmark uses).
const SOURCE_MOVED_CODE: &str = "SEMANTIC_SOURCE_MOVED";

/// Factual counters for acceptance measurement. Never a health score.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SemanticStats {
    /// Backend families whose install located and whose launcher is
    /// registered (registration is not a start).
    pub registered: Vec<&'static str>,
    /// Families that are not usable, with the exact reason.
    pub unavailable: BTreeMap<&'static str, String>,
    /// Queries that ran the structural demand probe.
    pub probes: u64,
    /// Probes whose canonical gaps demanded semantic refresh of at least
    /// one servable, not-current owner.
    pub demands: u64,
    pub owners_refreshed: u64,
    pub owner_failures: u64,
    /// Lease acquisitions refused (backend failed, backoff, degraded).
    pub start_failures: u64,
    /// Change batches delivered to a warm backend before a refresh.
    pub change_notifications: u64,
    /// Supervisor fleet counters.
    pub backend_starts: u64,
    pub backend_start_successes: u64,
    pub backend_requests: u64,
    pub backend_crashes: u64,
    pub backend_restarts: u64,
    pub live_runtimes: usize,
    pub idle_unloads: u64,
}

enum Backend {
    Python {
        settings: PythonSettings,
        install: PyrightInstall,
    },
    TypeScript {
        install: TypeScriptInstall,
        launcher: Arc<TypeScriptLauncher>,
    },
    Svelte {
        install: SvelteInstall,
    },
}

impl Backend {
    const fn name(&self) -> &'static str {
        match self {
            Self::Python { .. } => "python",
            Self::TypeScript { .. } => "typescript",
            Self::Svelte { .. } => "svelte",
        }
    }

    fn serves(&self, language: ResourceLanguage) -> bool {
        match self {
            Self::Python { .. } => language == ResourceLanguage::Python,
            Self::TypeScript { .. } => typescript::lifecycle::SERVED_LANGUAGES.contains(&language),
            Self::Svelte { .. } => language == ResourceLanguage::Svelte,
        }
    }
}

/// What one running backend has been shown of the Workspace: the
/// supervisor's start count when it was shown, and each active
/// Resource's locator and revision.
type BackendView = (u64, BTreeMap<ResourceId, (String, String)>);

/// One Resource change the backend has not been told about yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Added,
    Changed,
    Moved,
    Deleted,
}

pub struct WorkspaceSemantic {
    workspace: WorkspaceId,
    root: PathBuf,
    index_db: PathBuf,
    supervisor: SemanticRuntimeSupervisor,
    backends: Vec<Backend>,
    /// Per family, in `backends` order.
    views: Vec<Option<BackendView>>,
    stats: SemanticStats,
}

impl WorkspaceSemantic {
    /// Register every backend family the Workspace/global config locates.
    /// Reads config and install manifests only: no process is started.
    pub fn load(
        global_paths: &GlobalPaths,
        workspace_config: &WorkspaceConfig,
        workspace: WorkspaceId,
        root: &Path,
        index_db: &Path,
    ) -> Self {
        let mut stats = SemanticStats::default();
        let global = if global_paths.config_file.is_file() {
            load_global_config(global_paths)
                .map(|config| config.semantic_backends)
                .map_err(|error| error.to_string())
        } else {
            Ok(SemanticBackendLocators::default())
        };
        let global = global.unwrap_or_else(|error| {
            // An unreadable global config registers nothing -- it never
            // falls back to guessing where a backend might be.
            stats
                .unavailable
                .insert("global config", format!("unreadable: {error}"));
            SemanticBackendLocators::default()
        });
        let locators = &workspace_config.semantic_backends;

        let mut supervisor = SemanticRuntimeSupervisor::new(RuntimePolicy::default());
        let mut backends = Vec::new();

        match pick(&locators.python, &global.python)
            .ok_or_else(no_locator)
            .and_then(|locator: NodeBackendLocator| {
                let install_root = absolute(&locator.install_root)?;
                PyrightInstall::locate(install_root, locator.node.as_deref().unwrap_or("node"))
                    .map_err(|error| error.to_string())
            }) {
            Ok(install) => {
                let settings = PythonSettings::default();
                supervisor = supervisor.with_backend(Arc::new(PythonLauncher::new(
                    install.clone(),
                    root.to_path_buf(),
                    settings.clone(),
                )));
                backends.push(Backend::Python { settings, install });
            }
            Err(reason) => {
                stats.unavailable.insert("python", reason);
            }
        }

        match pick(&locators.typescript, &global.typescript)
            .ok_or_else(no_locator)
            .and_then(|locator: InstallRootLocator| {
                TypeScriptInstall::locate(absolute(&locator.install_root)?)
                    .map_err(|error| error.to_string())
            }) {
            Ok(install) => {
                let launcher = Arc::new(TypeScriptLauncher::new(install.clone()));
                supervisor = supervisor.with_backend(launcher.clone());
                backends.push(Backend::TypeScript { install, launcher });
            }
            Err(reason) => {
                stats.unavailable.insert("typescript", reason);
            }
        }

        match pick(&locators.svelte, &global.svelte)
            .ok_or_else(no_locator)
            .and_then(|locator: NodeBackendLocator| {
                let install = SvelteInstall::locate(absolute(&locator.install_root)?)
                    .map_err(|error| error.to_string())?;
                Ok((install, locator.node.unwrap_or_else(|| "node".to_owned())))
            }) {
            Ok((install, node)) => {
                supervisor =
                    supervisor.with_backend(Arc::new(SvelteLauncher::new(install.clone(), node)));
                backends.push(Backend::Svelte { install });
            }
            Err(reason) => {
                stats.unavailable.insert("svelte", reason);
            }
        }

        // C# and Rust Level A both load the project (MSBuild evaluation;
        // Cargo build scripts / proc macros), which `trust.rs` gates on an
        // explicit `ProjectExecutionTrust::Trusted` decision. The product
        // default is Untrusted and nothing infers otherwise, so these
        // families are reported, never started.
        let csharp: Option<InstallRootLocator> = pick(&locators.csharp, &global.csharp);
        let rust: Option<ExecutableLocator> = pick(&locators.rust, &global.rust);
        for (name, configured) in [("csharp", csharp.is_some()), ("rust", rust.is_some())] {
            stats.unavailable.insert(
                name,
                if configured {
                    "Level A requires ProjectExecutionTrust::Trusted; the product default is \
                     Untrusted and no trust decision exists"
                        .to_owned()
                } else {
                    no_locator()
                },
            );
        }

        stats.registered = backends.iter().map(Backend::name).collect();
        Self {
            workspace,
            root: root.to_path_buf(),
            index_db: index_db.to_path_buf(),
            supervisor,
            views: backends.iter().map(|_| None).collect(),
            backends,
            stats,
        }
    }

    #[must_use]
    pub fn stats(&self) -> SemanticStats {
        let fleet = self.supervisor.fleet_telemetry();
        SemanticStats {
            backend_starts: fleet.starts_attempted,
            backend_start_successes: fleet.starts_succeeded,
            backend_requests: fleet.requests_started,
            backend_crashes: fleet.crashes,
            backend_restarts: fleet.restart_attempts,
            live_runtimes: fleet.live_runtimes,
            ..self.stats.clone()
        }
    }

    /// The existing idle policy: retire warm runtimes nobody uses.
    pub fn tick(&mut self) {
        self.stats.idle_unloads += self.supervisor.sweep_idle() as u64;
    }

    /// Enrich the owners the target's canonical gaps prove need semantic
    /// resolution, then return; the caller runs the original operation.
    ///
    /// Every failure is absorbed: the structural answer and its explicit
    /// gaps are what the query reports then, never a false zero.
    pub fn enrich(&mut self, surface: &CoreQuerySurface, target: &ProjectionTarget) {
        if self.backends.is_empty() {
            return;
        }
        self.stats.probes += 1;
        let owners = match self.demanded_owners(surface, target) {
            Ok(owners) => owners,
            Err(error) => {
                eprintln!(
                    "brainprintd: workspace {} semantic demand probe failed: {error}",
                    self.workspace
                );
                return;
            }
        };
        if owners.values().all(BTreeSet::is_empty) {
            return;
        }
        self.stats.demands += 1;
        for (family, owners) in owners {
            if owners.is_empty() {
                continue;
            }
            if let Err(error) = self.refresh(family, &owners) {
                eprintln!(
                    "brainprintd: workspace {} {} semantic refresh failed: {error}",
                    self.workspace,
                    self.backends[family].name()
                );
            }
        }
    }

    /// Owner Resources, grouped by registered family, whose persisted
    /// gaps or coverage for this exact target require semantics and whose
    /// semantic publication is not already current.
    fn demanded_owners(
        &self,
        surface: &CoreQuerySurface,
        target: &ProjectionTarget,
    ) -> Result<BTreeMap<usize, BTreeSet<ResourceId>>, String> {
        let probe = surface
            .relations(RelationsRequest {
                context: QueryContext {
                    workspace: self.workspace,
                    correlation: None,
                },
                target: target.clone(),
                direction: RelationDirection::Both,
                kinds: Vec::new(),
            })
            .map_err(|error| error.to_string())?;
        // Only an exact, current selection is a demand; anything else is
        // answered structurally with its own explicit gap.
        if !matches!(probe.target, TargetResolution::Resolved(_))
            || probe.currentness != Currentness::Current
        {
            return Ok(BTreeMap::new());
        }
        let mut resources = BTreeSet::new();
        for answer in &probe.answers {
            for gap in &answer.gaps {
                if gap.reason.requires_semantics() || gap.resolution == Resolution::Candidate {
                    resources.insert(gap.location.resource);
                }
            }
            // The queried scope's own coverage: semantic enrichment that
            // is not current, or a Resource the structural tier covers
            // only partially (`PARTIAL_SUPPORT`, e.g. a container-only
            // component) -- both already say the answer is incomplete.
            if let Some(scope) = answer.coverage.scope
                && (answer.coverage.semantic.not_current || scope.support != Support::Supported)
            {
                resources.insert(scope.resource);
            }
        }
        if resources.is_empty() {
            return Ok(BTreeMap::new());
        }

        let store = ResourceStore::open(&self.index_db).map_err(|error| error.to_string())?;
        let index = SemanticIndex::open(&self.index_db).map_err(|error| error.to_string())?;
        let mut demanded: BTreeMap<usize, BTreeSet<ResourceId>> = BTreeMap::new();
        for resource in resources {
            let Some(language) = store
                .get_by_id(resource)
                .map_err(|error| error.to_string())?
                .and_then(|resource| resource.language)
            else {
                continue;
            };
            let Some(family) = self
                .backends
                .iter()
                .position(|backend| backend.serves(language))
            else {
                // No usable backend for this language: the gap stays.
                continue;
            };
            let context = self.context(family, &index)?;
            let owner = SemanticOwner::new(context.context_key(), resource);
            if index
                .status(&owner)
                .map_err(|error| error.to_string())?
                .state
                != SemanticState::Current
            {
                demanded.entry(family).or_default().insert(resource);
            }
        }
        Ok(demanded)
    }

    /// The family's `AnalysisContext`, from the same inputs I4 acceptance
    /// derives it from. Deterministic and start-free.
    fn context(&self, family: usize, index: &SemanticIndex) -> Result<AnalysisContext, String> {
        let root = &self.root;
        let (backend, language, toolchain) = match &self.backends[family] {
            Backend::Python { settings, install } => (
                SemanticBackendKind::Python,
                ResourceLanguage::Python,
                python::toolchain_identity(
                    install,
                    &python::lifecycle::environment_identity(root, settings),
                ),
            ),
            Backend::TypeScript { install, .. } => (
                SemanticBackendKind::TypeScriptJavaScript,
                ResourceLanguage::TypeScript,
                typescript::toolchain_identity(
                    install,
                    &typescript::lifecycle::environment_identity(
                        index.connection(),
                        root,
                        "",
                        &install.manifest_version,
                    )
                    .map_err(|error| error.to_string())?,
                ),
            ),
            Backend::Svelte { install } => (
                SemanticBackendKind::Svelte,
                ResourceLanguage::Svelte,
                svelte::toolchain_identity(
                    install,
                    &svelte::lifecycle::environment_identity(index.connection(), root, "", install)
                        .map_err(|error| error.to_string())?,
                ),
            ),
        };
        Ok(AnalysisContext {
            workspace: self.workspace,
            backend,
            language,
            // The project is the Workspace root: a normalized key, never
            // an absolute path.
            project_root: ProjectRootIdentity::Key(String::new()),
            toolchain,
        })
    }

    fn refresh(&mut self, family: usize, resources: &BTreeSet<ResourceId>) -> Result<(), String> {
        let index = SemanticIndex::open(&self.index_db).map_err(|error| error.to_string())?;
        let context = self.context(family, &index)?;
        let owners: BTreeSet<SemanticOwner> = resources
            .iter()
            .map(|resource| SemanticOwner::new(context.context_key(), *resource))
            .collect();
        let binding = AnalysisContextBinding {
            context: context.clone(),
            // Python resolves this against its launcher's Workspace root;
            // the TS/Svelte launchers take the root itself.
            project_root_rel: match &self.backends[family] {
                Backend::Python { .. } => String::new(),
                _ => self.root.to_string_lossy().into_owned(),
            },
            config_file_rel: None,
        };
        let lease = match self.supervisor.acquire(&binding) {
            Ok(lease) => lease,
            Err(failure) => {
                // Absent/crashed/backoff/degraded: the structural answer
                // and its gaps stand; the supervisor bounds restarts.
                self.stats.start_failures += 1;
                return Err(failure.to_string());
            }
        };
        let options = RequestOptions::with_timeout(REQUEST_TIMEOUT);
        let root = self.root.as_path();

        // The existing "tell the backend" step: a warm process has its own
        // file snapshot, so every Resource change since it was last shown
        // the Workspace is delivered before it is asked anything. A process
        // the supervisor (re)started read the current files itself.
        let starts = self
            .supervisor
            .telemetry(&context.context_key())
            .map_or(0, |telemetry| telemetry.starts_succeeded);
        let current = resource_view(&self.index_db)?;
        let pending = match &self.views[family] {
            Some((seen, shown)) if *seen == starts => Some(diff(shown, &current)),
            _ => None,
        };
        match (&self.backends[family], &pending) {
            (_, Some(changes)) if changes.is_empty() => {}
            (Backend::Python { .. }, Some(changes)) => {
                let changes: Vec<_> = changes
                    .iter()
                    .map(|(resource, kind, path, previous)| {
                        let mut change = python::lifecycle::ResourceChange::new(
                            *resource,
                            match kind {
                                Change::Added => python::ChangeKind::Added,
                                Change::Changed => python::ChangeKind::Changed,
                                Change::Moved => python::ChangeKind::Moved,
                                Change::Deleted => python::ChangeKind::Deleted,
                            },
                            path.clone(),
                        );
                        change.previous_path_rel.clone_from(previous);
                        change
                    })
                    .collect();
                python::adapter::notify_watched_files(
                    &python::LeaseQueries::new(&lease, options),
                    python::lifecycle::watched_changes(root, &changes),
                )
                .map_err(|error| error.to_string())?;
            }
            (Backend::TypeScript { .. }, Some(changes)) => {
                let changes: Vec<_> = changes
                    .iter()
                    .map(|(resource, kind, path, previous)| {
                        let mut change = typescript::lifecycle::ResourceChange::new(
                            *resource,
                            match kind {
                                Change::Added => typescript::lifecycle::ChangeKind::Added,
                                Change::Changed => typescript::lifecycle::ChangeKind::Changed,
                                Change::Moved => typescript::lifecycle::ChangeKind::Moved,
                                Change::Deleted => typescript::lifecycle::ChangeKind::Deleted,
                            },
                            path.clone(),
                        );
                        change.previous_path_rel.clone_from(previous);
                        change
                    })
                    .collect();
                typescript::lifecycle::synchronize(
                    &typescript::LeaseQueries::new(&lease, options),
                    root,
                    &changes,
                )
                .map_err(|error| error.to_string())?;
            }
            (Backend::Svelte { .. }, Some(changes)) => {
                let changes: Vec<_> = changes
                    .iter()
                    .map(|(resource, kind, path, previous)| {
                        let mut change = svelte::lifecycle::ResourceChange::new(
                            *resource,
                            match kind {
                                Change::Added => svelte::lifecycle::ChangeKind::Added,
                                Change::Changed => svelte::lifecycle::ChangeKind::Changed,
                                Change::Moved => svelte::lifecycle::ChangeKind::Moved,
                                Change::Deleted => svelte::lifecycle::ChangeKind::Deleted,
                            },
                            path.clone(),
                        );
                        change.previous_path_rel.clone_from(previous);
                        change
                    })
                    .collect();
                svelte::lifecycle::synchronize(
                    &svelte::LeaseQueries::new(&lease, options),
                    root,
                    &changes,
                )
                .map_err(|error| error.to_string())?;
            }
            (Backend::Svelte { .. }, None) => {
                // The existing restart barrier: a component the server was
                // never told about is an error rather than an empty answer.
                svelte::lifecycle::announce_components(
                    &index,
                    &svelte::LeaseQueries::new(&lease, options),
                    root,
                )
                .map_err(|error| error.to_string())?;
            }
            (_, None) => {}
        }
        if pending.as_ref().is_some_and(|changes| !changes.is_empty()) {
            self.stats.change_notifications += 1;
        }
        self.views[family] = Some((starts, current));

        let (refreshed, failed) = match &self.backends[family] {
            Backend::Python { settings, .. } => {
                let queries = python::LeaseQueries::new(&lease, options);
                let config = python::lifecycle::discover_config(index.connection(), root, "")
                    .map_err(|error| error.to_string())?
                    .basis(settings);
                let capabilities = python::capability_report(&context);
                let outcomes = python::lifecycle::refresh_owners(
                    &index,
                    &queries,
                    &mut python::RefreshRequest {
                        context: &context,
                        workspace_root: root,
                        owner: *resources.first().expect("non-empty"),
                        config: &config,
                        capabilities: &capabilities,
                        policy: python::BatchPolicy {
                            max_attempts: PYTHON_BATCH_ATTEMPTS,
                        },
                    },
                    &owners,
                )
                .map_err(|error| error.to_string())?;
                count(
                    outcomes
                        .iter()
                        .map(python::lifecycle::OwnerOutcome::succeeded),
                )
            }
            Backend::TypeScript { launcher, .. } => {
                let queries = typescript::LeaseQueries::new(&lease, options);
                let encoding = launcher
                    .negotiated_encoding()
                    .ok_or("the TypeScript handshake settled no position encoding")?;
                let config = typescript::lifecycle::discover_config(index.connection(), root, "")
                    .map_err(|error| error.to_string())?
                    .basis();
                let capabilities = typescript::capability_report(&context);
                let outcomes = typescript::lifecycle::refresh_owners(
                    &index,
                    &queries,
                    &mut typescript::RefreshRequest {
                        context: &context,
                        workspace_root: root,
                        owner: *resources.first().expect("non-empty"),
                        config: &config,
                        capabilities: &capabilities,
                        encoding,
                    },
                    &owners,
                )
                .map_err(|error| error.to_string())?;
                count(
                    outcomes
                        .iter()
                        .map(typescript::lifecycle::OwnerOutcome::succeeded),
                )
            }
            Backend::Svelte { .. } => {
                let queries = svelte::LeaseQueries::new(&lease, options);
                let config = svelte::lifecycle::discover_config(index.connection(), "")
                    .map_err(|error| error.to_string())?
                    .basis();
                let capabilities = svelte::capability_report(&context);
                let outcomes = svelte::lifecycle::refresh_owners(
                    &index,
                    &queries,
                    &mut svelte::RefreshRequest {
                        context: &context,
                        workspace_root: root,
                        owner: *resources.first().expect("non-empty"),
                        config: &config,
                        capabilities: &capabilities,
                    },
                    &owners,
                )
                .map_err(|error| error.to_string())?;
                count(
                    outcomes
                        .iter()
                        .map(svelte::lifecycle::OwnerOutcome::succeeded),
                )
            }
        };
        self.stats.owners_refreshed += refreshed;
        self.stats.owner_failures += failed;
        Ok(())
    }
}

impl Drop for WorkspaceSemantic {
    fn drop(&mut self) {
        self.supervisor.shutdown();
    }
}

/// Withdraw every semantic contribution and mark its owner not current,
/// ahead of a structural publication (#39 "Structural mutation
/// interaction").
///
/// The I4 contract: withdrawal must precede structural replacement,
/// because `semantic_evidence.relation_id` deliberately has no cascade
/// and a re-resolved dependent would otherwise fail the publication.
/// Withdrawing restores the displaced structural gaps, so the next
/// semantic-required query demands a lazy refresh again.
///
/// ponytail: withdraws every owner rather than the per-language
/// `plan_changes` set; that only costs a re-proof on the next demand.
/// Narrow it per family if warm reuse across unrelated edits matters.
pub fn withdraw_all(index_db: &Path) -> Result<usize, String> {
    let index = SemanticIndex::open(index_db).map_err(|error| error.to_string())?;
    let connection = index.connection();
    let contexts: Vec<String> = connection
        .prepare("SELECT DISTINCT context_key FROM semantic_publication")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()
        })
        .map_err(|error| error.to_string())?;
    let mut withdrawn = 0;
    for context_key in contexts {
        let owners = index
            .owners_of_context(&context_key)
            .map_err(|error| error.to_string())?;
        let transaction = connection
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        merge::withdraw(&transaction, &context_key, None).map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        for owner in &owners {
            if index
                .status(owner)
                .map_err(|error| error.to_string())?
                .state
                == SemanticState::Current
            {
                index
                    .mark_dirty(owner, SOURCE_MOVED_CODE)
                    .map_err(|error| error.to_string())?;
                withdrawn += 1;
            }
        }
    }
    Ok(withdrawn)
}

/// #39 precedence: the Workspace's explicit locator, else the global one.
fn pick<T: Clone>(workspace: &Option<T>, global: &Option<T>) -> Option<T> {
    workspace.clone().or_else(|| global.clone())
}

/// Every active Resource's locator and revision, by identity.
///
/// ponytail: a full inventory read per refresh batch (only on proven
/// demand); journal-driven change sets if large Workspaces make it show.
fn resource_view(index_db: &Path) -> Result<BTreeMap<ResourceId, (String, String)>, String> {
    Ok(ResourceStore::open(index_db)
        .and_then(|store| store.list_active())
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|resource| (resource.id, (resource.path_rel, resource.resource_revision)))
        .collect())
}

/// What changed between what a backend was shown and the index now.
fn diff(
    shown: &BTreeMap<ResourceId, (String, String)>,
    current: &BTreeMap<ResourceId, (String, String)>,
) -> Vec<(ResourceId, Change, String, Option<String>)> {
    let mut changes = Vec::new();
    for (resource, (path, revision)) in current {
        match shown.get(resource) {
            None => changes.push((*resource, Change::Added, path.clone(), None)),
            Some((old_path, _)) if old_path != path => changes.push((
                *resource,
                Change::Moved,
                path.clone(),
                Some(old_path.clone()),
            )),
            Some((_, old_revision)) if old_revision != revision => {
                changes.push((*resource, Change::Changed, path.clone(), None));
            }
            Some(_) => {}
        }
    }
    for (resource, (path, _)) in shown {
        if !current.contains_key(resource) {
            changes.push((*resource, Change::Deleted, path.clone(), Some(path.clone())));
        }
    }
    changes
}

fn no_locator() -> String {
    "no backend locator in the Workspace or global config".to_owned()
}

/// #39: an install locator is an absolute filesystem path, never one
/// relative to the Workspace or the daemon's working directory.
fn absolute(path: &Path) -> Result<&Path, String> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(format!(
            "backend locator {} is not an absolute path",
            path.display()
        ))
    }
}

fn count(succeeded: impl Iterator<Item = bool>) -> (u64, u64) {
    succeeded.fold((0, 0), |(ok, failed), success| {
        if success {
            (ok + 1, failed)
        } else {
            (ok, failed + 1)
        }
    })
}
