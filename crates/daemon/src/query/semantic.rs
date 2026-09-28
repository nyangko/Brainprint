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
    sync::{Arc, Mutex, PoisonError, Weak},
    time::Duration,
};

use brainprint_core::{ResourceId, WorkspaceId};
use brainprint_engine::{
    config::{
        ExecutableLocator, InstallRootLocator, NodeBackendLocator, SemanticBackendLocators,
        WorkspaceConfig, load_global_config,
    },
    csharp_semantic::{self as csharp, CSharpHost, CSharpInstall, CSharpLauncher},
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
    runtime::{
        HostError, RequestOptions, RuntimePolicy, SemanticBackendLauncher, SemanticRuntimeHost,
        SemanticRuntimeSupervisor,
    },
    rust_semantic::{self as rust, RustHost, RustInstall, RustLauncher},
    semantic::{AnalysisContext, AnalysisContextBinding, ProjectRootIdentity, SemanticBackendKind},
    semantic_index::{SemanticIndex, SemanticOwner, SemanticState},
    svelte_semantic::{self as svelte, SvelteInstall, SvelteLauncher},
    trust::ProjectExecutionTrust,
    typescript_semantic::{self as typescript, TypeScriptInstall, TypeScriptLauncher},
};

/// Per-request backend timeout: the value the I4 real-backend acceptance
/// uses (a tuning value, not a contract).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a C# project load / a Rust workspace settle may take: the
/// I4 real-backend acceptance values (tuning, not contract).
const CSHARP_LOAD_TIMEOUT: Duration = Duration::from_secs(180);
const RUST_SETTLE_TIMEOUT: Duration = Duration::from_secs(240);

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
    /// Registered only under an explicit Workspace `Trusted` decision.
    CSharp {
        install: CSharpInstall,
        launched: Arc<Launched<CSharpLauncher, CSharpHost>>,
    },
    /// Registered only under an explicit Workspace `Trusted` decision.
    Rust {
        install: RustInstall,
        launched: Arc<Launched<RustLauncher, RustHost>>,
    },
}

impl Backend {
    const fn name(&self) -> &'static str {
        match self {
            Self::Python { .. } => "python",
            Self::TypeScript { .. } => "typescript",
            Self::Svelte { .. } => "svelte",
            Self::CSharp { .. } => "csharp",
            Self::Rust { .. } => "rust",
        }
    }

    fn serves(&self, language: ResourceLanguage) -> bool {
        match self {
            Self::Python { .. } => language == ResourceLanguage::Python,
            Self::TypeScript { .. } => typescript::lifecycle::SERVED_LANGUAGES.contains(&language),
            Self::Svelte { .. } => language == ResourceLanguage::Svelte,
            Self::CSharp { .. } => language == ResourceLanguage::CSharp,
            Self::Rust { .. } => language == ResourceLanguage::Rust,
        }
    }
}

/// The existing C#/Rust launcher, as the supervisor sees it, remembering
/// the host it last started. The project-load / settle barrier and the
/// opened-document versions belong to that connection, which a lease
/// does not expose; I4 acceptance drives them on the host the same way.
struct Launched<L, H> {
    launcher: L,
    last: Mutex<Weak<H>>,
}

impl<L, H> Launched<L, H> {
    fn new(launcher: L) -> Self {
        Self {
            launcher,
            last: Mutex::new(Weak::new()),
        }
    }

    fn remember(&self, host: H) -> Arc<H> {
        let host = Arc::new(host);
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Arc::downgrade(&host);
        host
    }

    fn host(&self) -> Result<Arc<H>, String> {
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .upgrade()
            .ok_or_else(|| "the leased backend connection is gone".to_owned())
    }
}

impl SemanticBackendLauncher for Launched<CSharpLauncher, CSharpHost> {
    fn kind(&self) -> SemanticBackendKind {
        SemanticBackendKind::CSharp
    }

    fn launch(
        &self,
        binding: &AnalysisContextBinding,
    ) -> Result<Arc<dyn SemanticRuntimeHost>, HostError> {
        Ok(self.remember(self.launcher.start(binding)?))
    }
}

impl SemanticBackendLauncher for Launched<RustLauncher, RustHost> {
    fn kind(&self) -> SemanticBackendKind {
        SemanticBackendKind::Rust
    }

    fn launch(
        &self,
        binding: &AnalysisContextBinding,
    ) -> Result<Arc<dyn SemanticRuntimeHost>, HostError> {
        Ok(self.remember(self.launcher.start(binding)?))
    }
}

/// A C# connection's own state, for the existing lifecycle traits.
struct CSharpConnection<'a>(&'a CSharpHost);

impl csharp::lifecycle::DocumentVersions for CSharpConnection<'_> {
    fn next_document_version(&self) -> i64 {
        self.0.next_document_version()
    }

    fn exchange_text(&self, uri: &str, text: &str) -> Option<String> {
        self.0.exchange_text(uri, text)
    }
}

impl csharp::lifecycle::ProjectLoadBarrier for CSharpConnection<'_> {
    fn completions_seen(&self) -> u64 {
        self.0.load_completions()
    }

    fn wait_for_project_load(&self, seen: u64) -> Result<usize, String> {
        if self.0.wait_for_project_load(seen, CSHARP_LOAD_TIMEOUT) {
            Ok(1)
        } else {
            Err(format!(
                "the server did not announce project initialization within {CSHARP_LOAD_TIMEOUT:?}"
            ))
        }
    }
}

/// A Rust connection's own state, for the existing lifecycle traits.
struct RustConnection<'a>(&'a RustHost);

impl rust::lifecycle::DocumentVersions for RustConnection<'_> {
    fn next_document_version(&self) -> i64 {
        self.0.next_document_version()
    }

    fn exchange_text(&self, uri: &str, text: &str) -> Option<String> {
        self.0.exchange_text(uri, text)
    }
}

impl rust::lifecycle::QuiescenceBarrier for RustConnection<'_> {
    fn settlings_seen(&self) -> u64 {
        self.0.load_completions()
    }

    fn wait_for_quiescent(&self, seen: u64) -> Result<usize, String> {
        if self.0.wait_for_quiescent(seen, RUST_SETTLE_TIMEOUT) {
            Ok(1)
        } else {
            Err(format!(
                "the server did not settle within {RUST_SETTLE_TIMEOUT:?}"
            ))
        }
    }
}

/// What one running backend has been shown of the Workspace: the
/// supervisor's start count when it was shown, and each active
/// Resource's locator, revision and language.
type BackendView = (u64, BTreeMap<ResourceId, Shown>);

type Shown = (String, String, Option<ResourceLanguage>);

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
    /// The Workspace config's explicit decision; absent is Untrusted.
    trust: ProjectExecutionTrust,
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
        // explicit `ProjectExecutionTrust::Trusted` decision. The only
        // owner of that decision is this Workspace's own config; absent is
        // Untrusted, and an Untrusted family is reported, never started.
        let trust = workspace_config
            .project_execution_trust
            .unwrap_or_else(ProjectExecutionTrust::default_for_workspace);
        match trusted(trust, pick(&locators.csharp, &global.csharp)).and_then(
            |locator: InstallRootLocator| {
                CSharpInstall::locate(absolute(&locator.install_root)?)
                    .map_err(|error| error.to_string())
            },
        ) {
            Ok(install) => {
                let launched = Arc::new(Launched::new(CSharpLauncher::new(
                    install.clone(),
                    trust,
                    global_paths.logs_dir.join(format!("csharp-{workspace}")),
                )));
                supervisor = supervisor.with_backend(launched.clone());
                backends.push(Backend::CSharp { install, launched });
            }
            Err(reason) => {
                stats.unavailable.insert("csharp", reason);
            }
        }

        match trusted(trust, pick(&locators.rust, &global.rust)).and_then(
            |locator: ExecutableLocator| {
                RustInstall::at(absolute(&locator.executable)?).map_err(|error| error.to_string())
            },
        ) {
            Ok(install) => {
                let launched = Arc::new(Launched::new(RustLauncher::new(install.clone(), trust)));
                supervisor = supervisor.with_backend(launched.clone());
                backends.push(Backend::Rust { install, launched });
            }
            Err(reason) => {
                stats.unavailable.insert("rust", reason);
            }
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
            trust,
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
            // The trust mode is inside both fingerprints, so a trusted and
            // an untrusted analysis are different contexts.
            Backend::CSharp { install, .. } => (
                SemanticBackendKind::CSharp,
                ResourceLanguage::CSharp,
                csharp::toolchain_identity(
                    install,
                    &csharp::lifecycle::environment_identity(
                        install,
                        &self.csharp_projects(index)?,
                    )
                    .map_err(|error| error.to_string())?,
                ),
            ),
            Backend::Rust { install, .. } => (
                SemanticBackendKind::Rust,
                ResourceLanguage::Rust,
                rust::toolchain_identity(
                    install,
                    &rust::lifecycle::environment_identity(install, &self.rust_packages(index)?)
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

    fn csharp_projects(
        &self,
        index: &SemanticIndex,
    ) -> Result<csharp::CSharpProjectConfig, String> {
        csharp::lifecycle::discover_projects_under(index.connection(), self.trust, Some(&self.root))
            .map_err(|error| error.to_string())
    }

    fn rust_packages(
        &self,
        index: &SemanticIndex,
    ) -> Result<rust::lifecycle::RustProjectConfig, String> {
        rust::lifecycle::discover_packages_under(index.connection(), self.trust, Some(&self.root))
            .map_err(|error| error.to_string())
    }

    /// A Resource's language now, or as the backend last saw it (a
    /// deleted Resource is only in the latter).
    fn language_of(
        &self,
        family: usize,
        current: &BTreeMap<ResourceId, Shown>,
        resource: ResourceId,
    ) -> Option<ResourceLanguage> {
        current
            .get(&resource)
            .or_else(|| {
                self.views[family]
                    .as_ref()
                    .and_then(|(_, shown)| shown.get(&resource))
            })
            .and_then(|(_, _, language)| *language)
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
            // C#: a connection this daemon has not prepared loads the
            // projects (registration already required Trusted) and is
            // handed every source; a prepared one gets the change batch,
            // reloading first when project membership moved -- the I4
            // acceptance sequence, on the leased connection.
            (Backend::CSharp { launched, .. }, pending) => {
                let host = launched.host()?;
                let connection = CSharpConnection(&host);
                let queries = csharp::LeaseQueries::new(&lease, options);
                let changes: Vec<csharp::lifecycle::ResourceChange> = match pending {
                    None => {
                        let projects = self.csharp_projects(&index)?;
                        let seen = host.load_completions();
                        csharp::adapter::reopen_projects(
                            &queries,
                            projects
                                .project_files
                                .iter()
                                .filter(|project| project.path_key.ends_with(".csproj"))
                                .map(|project| {
                                    csharp::protocol::path_to_uri(&root.join(&project.path_key))
                                })
                                .collect(),
                        )
                        .map_err(|failure| failure.to_string())?;
                        csharp::lifecycle::ProjectLoadBarrier::wait_for_project_load(
                            &connection,
                            seen,
                        )?;
                        sources(&current, ResourceLanguage::CSharp)
                            .map(|(resource, path)| {
                                csharp::lifecycle::ResourceChange::new(
                                    resource,
                                    csharp::lifecycle::ChangeKind::Changed,
                                    path,
                                )
                            })
                            .collect()
                    }
                    Some(changes) => {
                        let changes: Vec<_> = changes
                            .iter()
                            .map(|(resource, kind, path, previous)| {
                                let mut change = csharp::lifecycle::ResourceChange::new(
                                    *resource,
                                    match kind {
                                        Change::Added => csharp::lifecycle::ChangeKind::Added,
                                        Change::Changed => csharp::lifecycle::ChangeKind::Changed,
                                        Change::Moved => csharp::lifecycle::ChangeKind::Moved,
                                        Change::Deleted => csharp::lifecycle::ChangeKind::Deleted,
                                    },
                                    path.clone(),
                                )
                                .with_language(self.language_of(family, &current, *resource));
                                change.previous_path_rel.clone_from(previous);
                                change
                            })
                            .collect();
                        if changes.iter().any(|change| {
                            change.class() == csharp::lifecycle::ChangeClass::ProjectStructure
                        }) {
                            csharp::lifecycle::reload_projects(
                                &queries,
                                &connection,
                                root,
                                &changes,
                                &self.csharp_projects(&index)?,
                            )
                            .map_err(|error| error.to_string())?;
                        }
                        changes
                    }
                };
                csharp::lifecycle::synchronize_documents(&queries, &connection, root, &changes)
                    .map_err(|error| error.to_string())?;
            }
            // Rust: rust-analyzer loads the Cargo workspace itself at
            // start; a fresh connection is waited on until it settles and
            // handed every source, a prepared one gets the batch (a
            // manifest change reloads first).
            (Backend::Rust { launched, .. }, pending) => {
                let host = launched.host()?;
                let connection = RustConnection(&host);
                let queries = rust::LeaseQueries::new(&lease, options);
                let changes: Vec<rust::lifecycle::ResourceChange> = match pending {
                    None => {
                        rust::lifecycle::QuiescenceBarrier::wait_for_quiescent(&connection, 0)?;
                        sources(&current, ResourceLanguage::Rust)
                            .map(|(resource, path)| {
                                rust::lifecycle::ResourceChange::new(
                                    resource,
                                    rust::lifecycle::ChangeKind::Changed,
                                    path,
                                )
                            })
                            .collect()
                    }
                    Some(changes) => {
                        let changes: Vec<_> = changes
                            .iter()
                            .map(|(resource, kind, path, previous)| {
                                let mut change = rust::lifecycle::ResourceChange::new(
                                    *resource,
                                    match kind {
                                        Change::Added => rust::lifecycle::ChangeKind::Added,
                                        Change::Changed => rust::lifecycle::ChangeKind::Changed,
                                        Change::Moved => rust::lifecycle::ChangeKind::Moved,
                                        Change::Deleted => rust::lifecycle::ChangeKind::Deleted,
                                    },
                                    path.clone(),
                                )
                                .with_language(self.language_of(family, &current, *resource));
                                change.previous_path_rel.clone_from(previous);
                                change
                            })
                            .collect();
                        if changes.iter().any(|change| {
                            change.class() == rust::lifecycle::ChangeClass::ProjectDefinition
                        }) {
                            rust::lifecycle::reload_projects(
                                &queries,
                                &connection,
                                root,
                                &changes,
                                &self.rust_packages(&index)?,
                            )
                            .map_err(|error| error.to_string())?;
                        }
                        changes
                    }
                };
                rust::lifecycle::synchronize_documents(&queries, &connection, root, &changes)
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
            Backend::CSharp { .. } => {
                let queries = csharp::LeaseQueries::new(&lease, options);
                let projects = self.csharp_projects(&index)?;
                let config = projects.basis();
                let capabilities = csharp::capability_report(&context, self.trust);
                let outcomes = csharp::lifecycle::refresh_owners(
                    &index,
                    &queries,
                    &mut csharp::RefreshRequest {
                        context: &context,
                        workspace_root: root,
                        owner: *resources.first().expect("non-empty"),
                        config: &config,
                        capabilities: &capabilities,
                        projects: &projects,
                    },
                    &owners,
                )
                .map_err(|error| error.to_string())?;
                count(
                    outcomes
                        .iter()
                        .map(csharp::lifecycle::OwnerOutcome::succeeded),
                )
            }
            Backend::Rust { .. } => {
                let queries = rust::LeaseQueries::new(&lease, options);
                let config = self.rust_packages(&index)?.basis();
                let capabilities = rust::capability_report(&context, self.trust);
                let outcomes = rust::lifecycle::refresh_owners(
                    &index,
                    &queries,
                    &mut rust::RefreshRequest {
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
                        .map(rust::lifecycle::OwnerOutcome::succeeded),
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

/// Every active Resource's locator, revision and language, by identity.
///
/// ponytail: a full inventory read per refresh batch (only on proven
/// demand); journal-driven change sets if large Workspaces make it show.
fn resource_view(index_db: &Path) -> Result<BTreeMap<ResourceId, Shown>, String> {
    Ok(ResourceStore::open(index_db)
        .and_then(|store| store.list_active())
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|resource| {
            (
                resource.id,
                (
                    resource.path_rel,
                    resource.resource_revision,
                    resource.language,
                ),
            )
        })
        .collect())
}

/// Every active source of one language: what a fresh connection is handed.
fn sources(
    current: &BTreeMap<ResourceId, Shown>,
    language: ResourceLanguage,
) -> impl Iterator<Item = (ResourceId, String)> + '_ {
    current
        .iter()
        .filter(move |(_, (_, _, shown))| *shown == Some(language))
        .map(|(resource, (path, _, _))| (*resource, path.clone()))
}

/// What changed between what a backend was shown and the index now.
fn diff(
    shown: &BTreeMap<ResourceId, Shown>,
    current: &BTreeMap<ResourceId, Shown>,
) -> Vec<(ResourceId, Change, String, Option<String>)> {
    let mut changes = Vec::new();
    for (resource, (path, revision, _)) in current {
        match shown.get(resource) {
            None => changes.push((*resource, Change::Added, path.clone(), None)),
            Some((old_path, _, _)) if old_path != path => changes.push((
                *resource,
                Change::Moved,
                path.clone(),
                Some(old_path.clone()),
            )),
            Some((_, old_revision, _)) if old_revision != revision => {
                changes.push((*resource, Change::Changed, path.clone(), None));
            }
            Some(_) => {}
        }
    }
    for (resource, (path, _, _)) in shown {
        if !current.contains_key(resource) {
            changes.push((*resource, Change::Deleted, path.clone(), Some(path.clone())));
        }
    }
    changes
}

fn no_locator() -> String {
    "no backend locator in the Workspace or global config".to_owned()
}

/// A project-loading backend's locator, only under an explicit Trusted.
fn trusted<T>(trust: ProjectExecutionTrust, locator: Option<T>) -> Result<T, String> {
    let locator = locator.ok_or_else(no_locator)?;
    if trust.may_load_projects() {
        Ok(locator)
    } else {
        Err(format!(
            "Level A requires ProjectExecutionTrust::Trusted; this Workspace is {trust} \
             (no project_execution_trust = \"Trusted\" in its config)"
        ))
    }
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
