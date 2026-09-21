//! Initial structural scan → staging → validation → atomic `STABLE`
//! publication of the Resource inventory baseline (#16 task 4).
//!
//! Scope: this is the *Resource inventory* baseline only. "Structural" here
//! stops at task 2's discovery/classification plus task 3's
//! fingerprint/identity rules; no Tree-sitter parse, Symbol, Occurrence, or
//! Relation exists yet (#16 task 7+). It also implements no watcher or
//! `change_journal` (#16 task 5) and no reconcile orchestration (#16 task
//! 6): the one scan this module runs is driven by its caller.
//!
//! ## Flow
//!
//! `discovery → observe(Verified) → plan_changes → begin BUILDING
//! generation → [ one transaction: basis recheck → snapshot revalidation →
//! Resource apply → invariant check → component_state → STABLE →
//! stable_generation_id swap ] → commit`.
//!
//! Nothing is written to the `resource` table while scanning. The candidate
//! [`ResourceChange`] list is process-local staging until the single
//! publication transaction, so Resource rows and the stable pointer can
//! never land in different commits -- the whole point of #16 task 4. There
//! is no second staging `index.db` file; the transaction *is* the staging
//! boundary.
//!
//! ## Why a second observation pass
//!
//! The clock's `current_workspace_revision` only advances when something
//! advances it, and the watcher that would do so is #16 task 5. So a basis
//! recheck alone cannot notice a file edited *during* the scan. Instead,
//! immediately before publishing, the Workspace is observed again in
//! [`ObservationMode::Verified`] and compared against the candidate
//! snapshot on the three things that decide correctness: the path set, the
//! classification (kind/role/language), and each FILE's content evidence
//! (size + content hash). `mtime` is deliberately not compared -- it is not
//! semantic input (#16 task 3). A mismatch is reported as a retryable
//! [`ScanError::InputChanged`]; retrying is the caller's explicit decision,
//! never an automatic loop.
//!
//! Correctness first: the second pass is a full re-hash. Narrowing it is a
//! benchmark question for later, not a contract change.

use std::{error::Error, fmt, path::Path};

use rusqlite::Connection;

use crate::{
    component::{self, ComponentError, ResourceIndexState},
    config::WorkspaceConfig,
    discovery::{self, DiscoveryError},
    generation::{self, GenerationError, GenerationRecord},
    identity::{self, IdentityError, ObservationMode, ObservedResource, ResourceChange},
    parser::ParseError,
    resource::{self, Resource, ResourceError, ResourceState, ResourceStore},
    schema, structural,
    symbol::SymbolError,
    watch::WatchError,
};

/// What a completed baseline publication produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanReport {
    /// The published generation, already `STABLE`.
    pub generation: GenerationRecord,
    /// The identity decisions this generation committed.
    pub changes: Vec<ResourceChange>,
}

/// Failure anywhere in a scan → publish path. Whatever the variant,
/// nothing is half-published: either the whole publication transaction
/// committed or none of it did, and the generation is left `ABORTED`.
///
/// Shared by the baseline scan (#16 task 4) and reconcile (#16 task 6):
/// both walk the same full-discovery → verify → publish path and fail in
/// exactly the same ways, so they report failure through one contract
/// rather than two parallel enums.
#[derive(Debug)]
pub enum ScanError {
    Discovery(DiscoveryError),
    Identity(IdentityError),
    Generation(GenerationError),
    Store(ResourceError),
    Component(ComponentError),
    /// Reading or updating the `change_journal` failed, or a stored row
    /// held a value outside the watcher's closed vocabulary.
    Journal(WatchError),
    /// A structural write failed (#16 task 14).
    Symbol(SymbolError),
    /// The current bytes of a Resource could not be parsed at all --
    /// distinct from a parse that merely came back PARTIAL.
    Parse(ParseError),
    /// The Workspace changed while the scan was running, so the candidate
    /// snapshot no longer describes the current filesystem. Retryable --
    /// by an explicit caller decision, never automatically.
    InputChanged {
        detail: String,
    },
    /// The Resource table did not satisfy its own invariants after the
    /// candidate changes were applied. Not retryable: the transaction is
    /// rolled back and this is a bug, not a race.
    InvariantViolated {
        detail: String,
    },
}

impl ScanError {
    /// Whether re-running the scan could plausibly succeed. Only an input
    /// race is retryable; the caller decides whether to retry.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::InputChanged { .. })
    }

    pub(crate) fn abort_reason(&self) -> String {
        match self {
            Self::InputChanged { detail } => {
                format!("workspace input changed during scan: {detail}")
            }
            Self::InvariantViolated { detail } => {
                format!("resource invariant violated: {detail}")
            }
            other => format!("baseline scan failed: {other}"),
        }
    }
}

impl fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Discovery(source) => write!(formatter, "workspace discovery failed: {source}"),
            Self::Identity(source) => write!(formatter, "resource identity failed: {source}"),
            Self::Generation(source) => {
                write!(formatter, "generation publication failed: {source}")
            }
            Self::Store(source) => write!(formatter, "resource store failed: {source}"),
            Self::Component(source) => write!(formatter, "component state failed: {source}"),
            Self::Journal(source) => write!(formatter, "change journal failed: {source}"),
            Self::Symbol(source) => write!(formatter, "structural write failed: {source}"),
            Self::Parse(source) => write!(formatter, "parse failed: {source}"),
            Self::InputChanged { detail } => write!(
                formatter,
                "workspace input changed during the scan, so nothing was published: {detail}"
            ),
            Self::InvariantViolated { detail } => write!(
                formatter,
                "resource invariant violated, so the publication was rolled back: {detail}"
            ),
        }
    }
}

impl Error for ScanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Discovery(source) => Some(source),
            Self::Identity(source) => Some(source),
            Self::Generation(source) => Some(source),
            Self::Store(source) => Some(source),
            Self::Component(source) => Some(source),
            Self::Journal(source) => Some(source),
            Self::Symbol(source) => Some(source),
            Self::Parse(source) => Some(source),
            Self::InputChanged { .. } | Self::InvariantViolated { .. } => None,
        }
    }
}

impl From<DiscoveryError> for ScanError {
    fn from(source: DiscoveryError) -> Self {
        Self::Discovery(source)
    }
}

impl From<IdentityError> for ScanError {
    fn from(source: IdentityError) -> Self {
        Self::Identity(source)
    }
}

impl From<GenerationError> for ScanError {
    fn from(source: GenerationError) -> Self {
        Self::Generation(source)
    }
}

impl From<ResourceError> for ScanError {
    fn from(source: ResourceError) -> Self {
        Self::Store(source)
    }
}

impl From<ComponentError> for ScanError {
    fn from(source: ComponentError) -> Self {
        Self::Component(source)
    }
}

impl From<WatchError> for ScanError {
    fn from(source: WatchError) -> Self {
        Self::Journal(source)
    }
}

impl From<SymbolError> for ScanError {
    fn from(source: SymbolError) -> Self {
        Self::Symbol(source)
    }
}

impl From<ParseError> for ScanError {
    fn from(source: ParseError) -> Self {
        Self::Parse(source)
    }
}

/// One Workspace's baseline scanner, bound to one `index.db` connection --
/// the Resource rows and the generation/clock rows it publishes together
/// must be written through the same connection to share one transaction.
pub struct BaselineScan {
    resources: ResourceStore,
}

impl BaselineScan {
    /// Open (creating/migrating if needed) the `index.db` at `path`.
    pub fn open(path: &Path) -> Result<Self, ScanError> {
        let opened = schema::index::open(path).map_err(ResourceError::from)?;
        Ok(Self::from_connection(opened.connection))
    }

    /// Wrap an already-opened `index.db` connection.
    #[must_use]
    pub fn from_connection(connection: Connection) -> Self {
        Self {
            resources: ResourceStore::from_connection(connection),
        }
    }

    /// The Resource inventory this scan publishes into.
    #[must_use]
    pub fn resources(&self) -> &ResourceStore {
        &self.resources
    }

    /// The current stable generation, or `None`. Never a `BUILDING` row.
    pub fn current_stable(&self) -> Result<Option<GenerationRecord>, ScanError> {
        Ok(generation::current_stable(self.connection())?)
    }

    /// The `RESOURCE_INDEX` component's persisted state, or `None` if no
    /// baseline has ever been published.
    pub fn resource_index_state(&self) -> Result<Option<ResourceIndexState>, ScanError> {
        Ok(component::read(self.connection())?)
    }

    /// Scan `workspace_root`, stage the resulting identity decisions, and
    /// publish them as one `STABLE` generation.
    ///
    /// `initial_workspace_revision` is only used to bootstrap
    /// `workspace_clock` when it has none yet; an existing clock is left
    /// untouched and its current revision becomes the generation's basis.
    /// No revision scheme is invented here -- the value is the caller's,
    /// under the existing revision contract.
    ///
    /// On any failure the publication transaction is rolled back, the
    /// generation is explicitly `ABORTED`, and both the Resource table and
    /// `stable_generation_id` are left exactly as they were.
    pub fn run_initial_scan(
        &self,
        workspace_root: &Path,
        config: &WorkspaceConfig,
        initial_workspace_revision: &str,
    ) -> Result<ScanReport, ScanError> {
        generation::bootstrap_clock(self.connection(), initial_workspace_revision)?;
        let basis = generation::current_workspace_revision(self.connection())?
            .ok_or(GenerationError::ClockNotBootstrapped)?;

        // Staging: scanned, planned, and held in process memory. Not one
        // row of this reaches the resource table before publication.
        let snapshot = self.observe_workspace(workspace_root, config, true)?;
        let active = self.resources.list_active()?;
        let changes = identity::plan_changes(&active, &snapshot, &[])?;

        let building = generation::begin_generation(self.connection(), &basis)?;
        match self.publish(building.id, workspace_root, config, &snapshot, &changes) {
            Ok(generation) => Ok(ScanReport {
                generation,
                changes,
            }),
            Err(error) => {
                // The transaction has already rolled back by the time this
                // runs; the generation row itself predates it, so the
                // abort is a separate, deliberate write.
                generation::abort_generation(
                    self.connection(),
                    building.id,
                    &error.abort_reason(),
                )?;
                Err(error)
            }
        }
    }

    /// The whole publication contract, in one transaction. Every early
    /// return drops `transaction`, which rolls back everything below.
    fn publish(
        &self,
        generation_id: i64,
        workspace_root: &Path,
        config: &WorkspaceConfig,
        snapshot: &[ObservedResource],
        changes: &[ResourceChange],
    ) -> Result<GenerationRecord, ScanError> {
        let transaction = self.resources.transaction()?;

        // 1-2. Still BUILDING, and the basis still matches the clock.
        generation::check_publishable(&transaction, generation_id)?;

        // Input revalidation: the basis recheck above cannot see a
        // filesystem edit, so re-observe and compare.
        let fresh = self.observe_workspace(workspace_root, config, false)?;
        if let Some(detail) = snapshot_drift(snapshot, &fresh) {
            return Err(ScanError::InputChanged { detail });
        }

        // 3. Resource changes, on this same connection and transaction.
        identity::apply_in_transaction(&self.resources, changes)?;

        // 4. The Resource table now agrees with what was published.
        verify_resource_invariants(&self.resources, snapshot)?;

        // 5. The structural index for every supported source file, in
        //    this same transaction: a Workspace that has Resources but no
        //    Symbols is exactly the half-current state #16 task 14 closes.
        let (building, grant) = generation::grant_publication(&transaction, generation_id)?;
        publish_structure(
            &transaction,
            &grant,
            &building.basis_workspace_revision,
            workspace_root,
            &self.resources.list_active()?,
        )?;

        // 6-8. Component state, then STABLE, then the pointer swap.
        component::mark_current(
            &transaction,
            &building.basis_workspace_revision,
            generation_id,
        )?;
        let published = generation::finish_publish_stable(&transaction, &building)?;

        // 8. One commit for Resource rows and the stable pointer alike.
        transaction.commit().map_err(ResourceError::from)?;
        Ok(published)
    }

    /// Observe every discovered entry in `workspace_root`.
    ///
    /// `use_persisted` decides whether the persisted row is offered to
    /// [`identity::observe`] at all. Both passes are
    /// [`ObservationMode::Verified`], so a FILE's bytes are hashed either
    /// way; withholding the persisted row on the revalidation pass keeps
    /// that pass from depending on the very rows it is validating.
    fn observe_workspace(
        &self,
        workspace_root: &Path,
        config: &WorkspaceConfig,
        use_persisted: bool,
    ) -> Result<Vec<ObservedResource>, ScanError> {
        if !use_persisted {
            return observe_workspace_verified(workspace_root, config);
        }
        discovery::enumerate_resources(workspace_root, config)?
            .iter()
            .map(|entry| {
                let previous = if use_persisted {
                    self.resources.get_active_by_path_key(&entry.path_key)?
                } else {
                    None
                };
                identity::observe(
                    workspace_root,
                    entry,
                    previous.as_ref(),
                    ObservationMode::Verified,
                )
                .map_err(ScanError::from)
            })
            .collect()
    }

    fn connection(&self) -> &Connection {
        self.resources.connection()
    }
}

/// Publish the structural index of every ACTIVE supported Resource in
/// `resources`, inside the caller's publication transaction (#16 task
/// 14).
///
/// Shared by the baseline scan (every Resource, because there is no
/// structure yet) and reconcile (only the Resources whose input actually
/// changed). A Resource whose parse is not complete keeps its previously
/// accepted Symbols as the last valid ones, and is recorded as such
/// rather than blanked or hidden.
pub(crate) fn publish_structure(
    connection: &Connection,
    grant: &generation::PublicationGrant,
    publication_revision: &str,
    workspace_root: &Path,
    resources: &[Resource],
) -> Result<(), ScanError> {
    for resource in resources {
        if resource.state != ResourceState::Active || resource.kind != resource::ResourceKind::File
        {
            continue;
        }
        structural::publish_resource(
            connection,
            grant,
            publication_revision,
            workspace_root,
            resource,
        )?;
    }
    Ok(())
}

/// Post-apply checks on the Resource table itself: the ACTIVE set is
/// exactly what was observed, and DELETED is exactly what is tombstoned.
///
/// Shared by the baseline scan and reconcile (#16 task 6) -- both apply
/// identity decisions inside a publication transaction and must not commit
/// a Resource table that disagrees with the snapshot they published.
pub(crate) fn verify_resource_invariants(
    resources: &ResourceStore,
    snapshot: &[ObservedResource],
) -> Result<(), ScanError> {
    let active = resources.list_active()?;
    if active.len() != snapshot.len() {
        return Err(ScanError::InvariantViolated {
            detail: format!(
                "{} ACTIVE resources for {} observed entries",
                active.len(),
                snapshot.len()
            ),
        });
    }

    for (persisted, observed) in active.iter().zip(snapshot) {
        if persisted.path_key != observed.discovered.path_key {
            return Err(ScanError::InvariantViolated {
                detail: format!(
                    "ACTIVE path {:?} is not the observed path {:?}",
                    persisted.path_key, observed.discovered.path_key
                ),
            });
        }
    }

    for stored in resources.list()? {
        let tombstoned = resource::is_tombstone_path_key(&stored.path_key);
        if (stored.state == crate::resource::ResourceState::Deleted) != tombstoned {
            return Err(ScanError::InvariantViolated {
                detail: format!(
                    "resource {:?} is {} but its path_key is {}a tombstone key",
                    stored.path_rel,
                    stored.state,
                    if tombstoned { "" } else { "not " }
                ),
            });
        }
    }

    Ok(())
}

/// Discover and observe the whole Workspace in
/// [`ObservationMode::Verified`], without consulting a single persisted
/// row: every FILE's current bytes are re-hashed, so the result describes
/// the filesystem and nothing else.
///
/// This is the correctness path's one way of looking at the Workspace --
/// the baseline scan's revalidation pass and every reconcile observation
/// (#16 task 6) go through it.
pub(crate) fn observe_workspace_verified(
    workspace_root: &Path,
    config: &WorkspaceConfig,
) -> Result<Vec<ObservedResource>, ScanError> {
    discovery::enumerate_resources(workspace_root, config)?
        .iter()
        .map(|entry| {
            identity::observe(workspace_root, entry, None, ObservationMode::Verified)
                .map_err(ScanError::from)
        })
        .collect()
}

/// The first way `candidate` and `fresh` disagree on something that decides
/// correctness -- path set, classification, or FILE content evidence -- or
/// `None` if they agree. `mtime` is not compared: it is not semantic input.
pub(crate) fn snapshot_drift(
    candidate: &[ObservedResource],
    fresh: &[ObservedResource],
) -> Option<String> {
    for (before, after) in candidate.iter().zip(fresh) {
        if before.discovered != after.discovered {
            return Some(format!(
                "classification or path changed at {:?}",
                after.discovered.path_rel
            ));
        }
        if before.size_bytes != after.size_bytes || before.content_hash != after.content_hash {
            return Some(format!(
                "content evidence changed at {:?}",
                after.discovered.path_rel
            ));
        }
    }

    // Both lists are path_key-ordered, so a length difference is the only
    // remaining way the path set can differ.
    match candidate.len().cmp(&fresh.len()) {
        std::cmp::Ordering::Less => Some(format!(
            "{:?} appeared during the scan",
            fresh[candidate.len()].discovered.path_rel
        )),
        std::cmp::Ordering::Greater => Some(format!(
            "{:?} disappeared during the scan",
            candidate[fresh.len()].discovered.path_rel
        )),
        std::cmp::Ordering::Equal => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use brainprint_core::ResourceId;

    use super::*;
    use crate::{
        component::{FreshnessState, ProcessingState},
        generation::GenerationState,
        resource::{Resource, ResourceKind, ResourceRole, ResourceState},
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    const INITIAL_REVISION: &str = "workspace-rev-1";

    /// A Workspace root plus an `index.db` outside it.
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-scan-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            fs::create_dir_all(&root).expect("workspace root should be creatable");
            Self { base, root }
        }

        fn db_path(&self) -> PathBuf {
            self.base.join("data").join("index.db")
        }

        fn scan_engine(&self) -> BaselineScan {
            BaselineScan::open(&self.db_path()).expect("index.db should open")
        }

        fn write(&self, rel: &str, contents: &str) {
            let full = self.root.join(rel);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).expect("parent dirs should be creatable");
            }
            fs::write(full, contents).expect("fixture file should be writable");
        }

        fn remove(&self, rel: &str) {
            fs::remove_file(self.root.join(rel)).expect("fixture file should be removable");
        }

        fn scan(&self, engine: &BaselineScan) -> Result<ScanReport, ScanError> {
            engine.run_initial_scan(&self.root, &WorkspaceConfig::default(), INITIAL_REVISION)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn active_paths(engine: &BaselineScan) -> Vec<String> {
        engine
            .resources()
            .list_active()
            .expect("list should succeed")
            .into_iter()
            .map(|resource| resource.path_rel)
            .collect()
    }

    #[test]
    fn a_fresh_workspace_scan_publishes_resources_and_a_stable_generation() {
        let fixture = Fixture::create("fresh");
        fixture.write("src/lib.rs", "fn main() {}");
        fixture.write("README.md", "# hi");
        let engine = fixture.scan_engine();

        let report = fixture.scan(&engine).expect("scan should publish");

        assert_eq!(report.generation.state, GenerationState::Stable);
        assert!(report.generation.published_at.is_some());
        assert_eq!(
            report.generation.basis_workspace_revision, INITIAL_REVISION,
            "the basis is the existing clock revision, not an invented one"
        );
        assert_eq!(
            active_paths(&engine),
            vec!["README.md", "src", "src/lib.rs"]
        );

        let current = engine
            .current_stable()
            .expect("current lookup should succeed")
            .expect("a stable generation should exist");
        assert_eq!(current.id, report.generation.id);
    }

    #[test]
    fn publication_marks_the_resource_index_component_ready_and_current() {
        let fixture = Fixture::create("component-state");
        fixture.write("src/lib.rs", "fn main() {}");
        let engine = fixture.scan_engine();

        let report = fixture.scan(&engine).expect("scan should publish");

        let state = engine
            .resource_index_state()
            .expect("component state should be readable")
            .expect("RESOURCE_INDEX should have been written");
        assert_eq!(state.processing_state, ProcessingState::Ready);
        assert_eq!(state.freshness_state, FreshnessState::Current);
        assert_eq!(state.stable_generation_id, Some(report.generation.id));
        assert_eq!(state.basis_workspace_revision, INITIAL_REVISION);
        assert_eq!(state.last_error_code, None);
    }

    #[test]
    fn a_building_generation_is_never_current() {
        let fixture = Fixture::create("building-not-current");
        fixture.write("src/lib.rs", "fn main() {}");
        let engine = fixture.scan_engine();
        generation::bootstrap_clock(engine.connection(), INITIAL_REVISION)
            .expect("bootstrap should succeed");

        generation::begin_generation(engine.connection(), INITIAL_REVISION)
            .expect("begin should succeed");

        assert!(
            engine
                .current_stable()
                .expect("current lookup should succeed")
                .is_none(),
            "a BUILDING generation must never be visible as current"
        );
        assert!(active_paths(&engine).is_empty());
    }

    #[test]
    fn no_source_text_is_stored_by_a_publication() {
        let fixture = Fixture::create("no-source-text");
        let secret = "fn unmistakable_source_body_marker() {}";
        fixture.write("src/lib.rs", secret);
        let engine = fixture.scan_engine();
        fixture.scan(&engine).expect("scan should publish");

        for resource in engine.resources().list().expect("list should succeed") {
            assert!(!resource.fingerprint.contains(secret));
            assert!(
                !resource
                    .content_hash
                    .as_deref()
                    .is_some_and(|hash| hash.contains(secret))
            );
        }

        let raw = fs::read(fixture.db_path()).expect("index.db should be readable");
        assert!(
            !raw.windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "no source text may be mirrored into index.db"
        );
    }

    #[test]
    fn an_unchanged_repeat_scan_keeps_resource_ids_and_revisions() {
        let fixture = Fixture::create("repeat");
        fixture.write("src/lib.rs", "fn main() {}");
        let engine = fixture.scan_engine();
        fixture.scan(&engine).expect("first scan should publish");
        let before = engine.resources().list_active().expect("list ok");

        let second = fixture.scan(&engine).expect("second scan should publish");

        assert!(
            second
                .changes
                .iter()
                .all(|change| matches!(change, ResourceChange::Unchanged { .. })),
            "an untouched Workspace must re-publish without changing a Resource"
        );
        assert_eq!(engine.resources().list_active().expect("list ok"), before);
        assert_eq!(
            engine
                .current_stable()
                .expect("current lookup ok")
                .expect("stable exists")
                .id,
            second.generation.id,
            "the newer generation is the one that is current"
        );
    }

    #[test]
    fn a_content_change_during_the_scan_aborts_without_publishing() {
        let fixture = Fixture::create("content-race");
        fixture.write("src/lib.rs", "fn main() {}");
        let engine = fixture.scan_engine();
        fixture.scan(&engine).expect("baseline scan should publish");
        let baseline = engine
            .current_stable()
            .expect("current ok")
            .expect("stable exists");
        let before = engine.resources().list_active().expect("list ok");

        // A candidate snapshot taken before the edit, published after it.
        let stale = engine
            .observe_workspace(&fixture.root, &WorkspaceConfig::default(), true)
            .expect("observation should succeed");
        fixture.write("src/lib.rs", "fn main() { let raced = 1; }");
        let building = generation::begin_generation(engine.connection(), INITIAL_REVISION)
            .expect("begin should succeed");
        let error = engine
            .publish(
                building.id,
                &fixture.root,
                &WorkspaceConfig::default(),
                &stale,
                &[],
            )
            .expect_err("a mid-scan content change must not publish");

        assert!(matches!(error, ScanError::InputChanged { .. }));
        assert!(error.is_retryable());
        assert_eq!(engine.resources().list_active().expect("list ok"), before);
        assert_eq!(
            engine
                .current_stable()
                .expect("current ok")
                .expect("stable exists")
                .id,
            baseline.id,
            "the existing stable generation must survive"
        );
    }

    #[test]
    fn a_path_appearing_or_disappearing_during_the_scan_fails_validation() {
        for (label, mutate) in [("created", true), ("deleted", false)] {
            let fixture = Fixture::create(&format!("path-race-{label}"));
            fixture.write("src/lib.rs", "fn main() {}");
            fixture.write("src/other.rs", "fn other() {}");
            let engine = fixture.scan_engine();
            generation::bootstrap_clock(engine.connection(), INITIAL_REVISION)
                .expect("bootstrap ok");

            let stale = engine
                .observe_workspace(&fixture.root, &WorkspaceConfig::default(), true)
                .expect("observation should succeed");
            if mutate {
                fixture.write("src/appeared.rs", "fn appeared() {}");
            } else {
                fixture.remove("src/other.rs");
            }

            let building = generation::begin_generation(engine.connection(), INITIAL_REVISION)
                .expect("begin ok");
            let error = engine
                .publish(
                    building.id,
                    &fixture.root,
                    &WorkspaceConfig::default(),
                    &stale,
                    &[],
                )
                .expect_err("a mid-scan path set change must not publish");

            assert!(
                matches!(error, ScanError::InputChanged { .. }),
                "{label}: expected an input-changed result, got {error:?}"
            );
            assert!(active_paths(&engine).is_empty());
            assert!(engine.current_stable().expect("current ok").is_none());
        }
    }

    #[test]
    fn an_obsolete_basis_revision_applies_nothing_and_aborts() {
        let fixture = Fixture::create("obsolete-basis");
        fixture.write("src/lib.rs", "fn main() {}");
        let engine = fixture.scan_engine();
        fixture.scan(&engine).expect("baseline scan should publish");
        let baseline = engine
            .current_stable()
            .expect("current ok")
            .expect("stable exists");
        let before = engine.resources().list_active().expect("list ok");

        let snapshot = engine
            .observe_workspace(&fixture.root, &WorkspaceConfig::default(), true)
            .expect("observation ok");
        let building =
            generation::begin_generation(engine.connection(), INITIAL_REVISION).expect("begin ok");
        // The Workspace revision moved on while this generation was built.
        engine
            .connection()
            .execute(
                "UPDATE workspace_clock SET current_workspace_revision = 'workspace-rev-2' \
                 WHERE id = 0",
                [],
            )
            .expect("revision advance should succeed");
        fixture.write("src/added.rs", "fn added() {}");

        let error = engine
            .publish(
                building.id,
                &fixture.root,
                &WorkspaceConfig::default(),
                &snapshot,
                &[],
            )
            .expect_err("a stale basis must not publish");

        assert!(matches!(
            error,
            ScanError::Generation(GenerationError::ObsoleteBasisRevision { .. })
        ));
        assert!(!error.is_retryable());
        assert_eq!(
            engine.resources().list_active().expect("list ok"),
            before,
            "no Resource change may be applied on a stale basis"
        );
        assert_eq!(
            engine
                .current_stable()
                .expect("current ok")
                .expect("stable exists")
                .id,
            baseline.id
        );
    }

    #[test]
    fn a_failed_resource_apply_rolls_back_everything_and_aborts_the_generation() {
        let fixture = Fixture::create("apply-failure");
        fixture.write("src/lib.rs", "fn main() {}");
        let engine = fixture.scan_engine();
        fixture.scan(&engine).expect("baseline scan should publish");
        let baseline = engine
            .current_stable()
            .expect("current ok")
            .expect("stable exists");

        // A Resource whose file does not exist: the next scan will plan a
        // delete for it. A decoy already sitting on the tombstone key that
        // delete must move it to makes the apply fail on the UNIQUE
        // constraint, mid-transaction.
        let ghost = Resource {
            id: ResourceId::generate(),
            path_rel: "src/ghost.rs".to_owned(),
            path_key: "src/ghost.rs".to_owned(),
            kind: ResourceKind::File,
            role: ResourceRole::Source,
            language: None,
            size_bytes: 0,
            mtime_ns: 0,
            fingerprint: "fp-ghost".to_owned(),
            content_hash: None,
            state: ResourceState::Active,
            resource_revision: "1".to_owned(),
            generated_kind: None,
            container_resource_id: None,
        };
        engine
            .resources()
            .insert_resource(&ghost)
            .expect("ghost insert ok");
        let collision = Resource {
            id: ResourceId::generate(),
            path_rel: "src/ghost.rs".to_owned(),
            path_key: format!(
                "{}{}/src/ghost.rs",
                resource::TOMBSTONE_PATH_KEY_PREFIX,
                hex_upper(&ghost.id.to_bytes())
            ),
            state: ResourceState::Deleted,
            ..ghost.clone()
        };
        engine
            .resources()
            .insert_resource(&collision)
            .expect("decoy insert ok");
        let before = engine.resources().list().expect("list ok");

        let error = fixture
            .scan(&engine)
            .expect_err("the apply must fail on the tombstone key collision");
        assert!(matches!(error, ScanError::Identity(_)));

        assert_eq!(
            engine.resources().list().expect("list ok"),
            before,
            "a failed apply must roll back every Resource write"
        );
        assert_eq!(
            engine
                .current_stable()
                .expect("current ok")
                .expect("stable exists")
                .id,
            baseline.id,
            "the previous stable generation must be preserved"
        );
        let building: Vec<i64> = {
            let mut statement = engine
                .connection()
                .prepare("SELECT id FROM generation WHERE state = 'BUILDING'")
                .expect("query should prepare");
            statement
                .query_map([], |row| row.get(0))
                .expect("query should run")
                .collect::<Result<_, _>>()
                .expect("rows should decode")
        };
        assert!(
            building.is_empty(),
            "the failed generation must be explicitly ABORTED, never left BUILDING"
        );
    }

    #[test]
    fn resource_rows_and_the_stable_pointer_become_visible_together() {
        let fixture = Fixture::create("atomic");
        fixture.write("src/lib.rs", "fn main() {}");
        let engine = fixture.scan_engine();

        // Before: neither the rows nor a stable pointer exist.
        assert!(active_paths(&engine).is_empty());
        assert!(engine.current_stable().expect("current ok").is_none());

        let report = fixture.scan(&engine).expect("scan should publish");

        // After: both, and the component state agrees on the generation.
        assert!(!active_paths(&engine).is_empty());
        assert_eq!(
            engine
                .current_stable()
                .expect("current ok")
                .expect("stable exists")
                .id,
            report.generation.id
        );
        assert_eq!(
            engine
                .resource_index_state()
                .expect("component state ok")
                .expect("component state exists")
                .stable_generation_id,
            Some(report.generation.id)
        );
    }

    #[test]
    fn a_published_baseline_survives_a_db_reopen() {
        let fixture = Fixture::create("reopen");
        fixture.write("src/lib.rs", "fn main() {}");
        let (published_id, before) = {
            let engine = fixture.scan_engine();
            let report = fixture.scan(&engine).expect("scan should publish");
            (
                report.generation.id,
                engine.resources().list_active().expect("list ok"),
            )
        };

        let reopened = fixture.scan_engine();
        assert_eq!(
            reopened
                .current_stable()
                .expect("current ok")
                .expect("stable survives reopen")
                .id,
            published_id
        );
        assert_eq!(
            reopened.resources().list_active().expect("list ok"),
            before,
            "Resource ids and revisions must survive a reopen"
        );
    }

    #[test]
    fn separate_worktree_index_dbs_stay_isolated() {
        let first = Fixture::create("worktree-a");
        let second = Fixture::create("worktree-b");
        first.write("src/lib.rs", "fn main() {}");
        second.write("src/lib.rs", "fn main() {}");
        second.write("src/only_here.rs", "fn only() {}");

        let first_engine = first.scan_engine();
        let second_engine = second.scan_engine();
        let first_report = first.scan(&first_engine).expect("first scan ok");
        let second_report = second.scan(&second_engine).expect("second scan ok");

        assert_eq!(active_paths(&first_engine), vec!["src", "src/lib.rs"]);
        assert_eq!(
            active_paths(&second_engine),
            vec!["src", "src/lib.rs", "src/only_here.rs"]
        );
        assert_eq!(first_report.generation.generation_no, 1);
        assert_eq!(
            second_report.generation.generation_no, 1,
            "each Workspace's index.db has its own generation sequence"
        );

        let first_id = first_engine
            .resources()
            .get_active_by_path_key("src/lib.rs")
            .expect("lookup ok")
            .expect("exists")
            .id;
        let second_id = second_engine
            .resources()
            .get_active_by_path_key("src/lib.rs")
            .expect("lookup ok")
            .expect("exists")
            .id;
        assert_ne!(first_id, second_id);
    }

    fn hex_upper(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02X}")).collect()
    }
}
