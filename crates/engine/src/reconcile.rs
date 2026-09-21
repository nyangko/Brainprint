//! The reconcile correctness path: recovering Resource correctness from
//! the current filesystem alone (#16 task 6).
//!
//! The watcher (#16 task 5) is a latency path whose journal is a pile of
//! *candidates*. Reconcile is the path that does not depend on it being
//! right, or on it existing at all: it re-discovers the whole Workspace,
//! observes every entry in [`ObservationMode::Verified`], and compares that
//! against the persisted ACTIVE inventory. A missed CREATE, a missed
//! DELETE, a missed MODIFY -- including a write that preserved both size
//! and mtime -- and a bulk change after a continuity loss are all found the
//! same way, because none of them is looked for: the filesystem is simply
//! re-read and believed.
//!
//! ## Flow
//!
//! `full discovery → Verified observation → compare with persisted ACTIVE →
//! confirmable journal move evidence → actual change decision → [ one
//! transaction: input recheck → Workspace revision advance → BUILDING
//! generation publishable → Resource apply → invariant check →
//! RESOURCE_INDEX CURRENT → journal APPLIED → reconcile bookkeeping →
//! STABLE → stable_generation_id swap ] → commit`.
//!
//! ## What the journal is used for
//!
//! Exactly one thing: [`MoveEvidence`]. A paired rename the backend itself
//! vouched for is the only way an identity survives a path change, and even
//! then [`identity::plan_changes`] accepts it only if it is unambiguous and
//! agrees with what was just observed. A half rename, a DELETE followed by
//! a CREATE, an ambiguous pairing, or a rename cycle produces no evidence
//! at all, so those paths resolve as an independent delete plus create: a
//! false split is recoverable, a false merge is not. Matching content hashes
//! are never evidence of a move.
//!
//! Nothing else in the journal is treated as fact. Reconcile's answer is
//! the same whether the journal is complete, stale, wrong, or empty.
//!
//! ## Workspace revision
//!
//! `current_workspace_revision` advances only when this verified comparison
//! confirms a real input change -- never on a raw event, never on wall
//! clock. The advance is the existing contract: `last_change_seq` is the
//! monotonic input-change sequence (#4 task 1 §4) and the revision is that
//! sequence, written in the same transaction as the generation that
//! publishes the change. No second revision scheme exists. A reconcile that
//! finds nothing advances neither the Workspace revision nor any
//! `resource_revision`.
//!
//! ## Continuity
//!
//! `watcher_continuity_state` is the watcher contract's, not this one's. A
//! successful reconcile proves the Resource inventory is current *now*; it
//! does not prove the event stream was repaired, so `LOST` stays `LOST`
//! until the watcher backend says otherwise. It is returned in
//! [`ReconcileReport::watcher_continuity`] rather than quietly cleared.

use std::path::Path;

use rusqlite::Connection;

use crate::{
    component::{self, ResourceIndexState},
    config::WorkspaceConfig,
    generation::{self, GenerationError, GenerationRecord},
    identity::{self, MoveEvidence, ObservedResource, ResourceChange},
    resource::{ResourceError, ResourceStore},
    scan::{self, ScanError},
    schema, structural,
    watch::{self, JournalEntry, WatcherContinuity},
};

/// What a completed reconcile established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    /// The generation this reconcile published, or `None` when the
    /// filesystem already agreed with the persisted inventory and there was
    /// nothing to publish.
    pub generation: Option<GenerationRecord>,
    /// The Workspace revision in force afterwards -- advanced only if
    /// [`Self::generation`] is `Some`.
    pub workspace_revision: String,
    /// Every identity decision the comparison produced, including the
    /// `Unchanged` ones.
    pub changes: Vec<ResourceChange>,
    /// The move evidence the journal could confirm and that was handed to
    /// [`identity::plan_changes`]. Identity accepts only the subset it can
    /// still prove against the observation, so an entry here is a candidate
    /// that was offered, not necessarily one that kept an id.
    pub move_evidence: Vec<MoveEvidence>,
    /// The journal was accounted for up to and including this `seq`; it is
    /// also what `workspace_clock.last_reconcile_seq` now holds. Rows
    /// written after it stay `PENDING`.
    pub journal_seq_through: i64,
    /// The watcher's own continuity state, untouched by this reconcile.
    pub watcher_continuity: WatcherContinuity,
}

impl ReconcileReport {
    /// Whether a new stable generation was published.
    #[must_use]
    pub fn published(&self) -> bool {
        self.generation.is_some()
    }
}

/// One Workspace's reconcile path, bound to one `index.db` connection --
/// the Resource rows, the clock, the journal, and the generation it
/// publishes together must be written through the same connection to share
/// one transaction.
pub struct Reconcile {
    resources: ResourceStore,
}

impl Reconcile {
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

    /// The Resource inventory this reconcile corrects.
    #[must_use]
    pub fn resources(&self) -> &ResourceStore {
        &self.resources
    }

    /// The current stable generation, or `None`. Never a `BUILDING` row.
    pub fn current_stable(&self) -> Result<Option<GenerationRecord>, ScanError> {
        Ok(generation::current_stable(self.connection())?)
    }

    /// The `RESOURCE_INDEX` component's persisted state.
    pub fn resource_index_state(&self) -> Result<Option<ResourceIndexState>, ScanError> {
        Ok(component::read(self.connection())?)
    }

    /// Every `change_journal` row, oldest first.
    pub fn journal(&self) -> Result<Vec<JournalEntry>, ScanError> {
        Ok(watch::journal(self.connection())?)
    }

    /// How far the last completed reconcile accounted for the journal.
    pub fn last_reconcile_seq(&self) -> Result<i64, ScanError> {
        Ok(generation::last_reconcile_seq(self.connection())?)
    }

    /// The current Workspace revision.
    pub fn workspace_revision(&self) -> Result<String, ScanError> {
        Ok(generation::current_workspace_revision(self.connection())?
            .ok_or(GenerationError::ClockNotBootstrapped)?)
    }

    /// Re-establish Resource correctness from the current filesystem.
    ///
    /// Publishes a new stable generation if -- and only if -- the verified
    /// comparison found an actual change, or no baseline has ever been
    /// published. On any failure the publication transaction is rolled
    /// back, a started generation is explicitly `ABORTED`, the previous
    /// stable generation and its Resource rows are left exactly as they
    /// were, `RESOURCE_INDEX` is left DIRTY rather than claimed CURRENT,
    /// and the journal keeps every candidate `PENDING`.
    pub fn run(
        &self,
        workspace_root: &Path,
        config: &WorkspaceConfig,
    ) -> Result<ReconcileReport, ScanError> {
        let basis = self.workspace_revision()?;
        let input = InputSnapshot {
            workspace_revision: basis.clone(),
            change_seq: generation::change_seq(self.connection())?,
            journal_seq: watch::max_journal_seq(self.connection())?,
        };

        // The filesystem is the truth being recovered; the journal only
        // contributes move evidence.
        let move_evidence = watch::pending_move_evidence(self.connection())?;
        let observed = scan::observe_workspace_verified(workspace_root, config)?;
        let active = self.resources.list_active()?;
        let changes = identity::plan_changes(&active, &observed, &move_evidence)?;

        // A Workspace with no baseline at all still needs one published,
        // so that "CURRENT" never means "current as of no generation".
        let publish = changes.iter().any(is_actual_change)
            || generation::current_stable(self.connection())?.is_none();

        let outcome = if publish {
            self.reconcile_changed(workspace_root, config, &input, &observed, &changes)
        } else {
            self.reconcile_no_op(workspace_root, config, &input, &observed, &changes)
                .map(|()| None)
        }?;

        Ok(ReconcileReport {
            workspace_revision: outcome.as_ref().map_or_else(
                || basis.clone(),
                |generation| generation.basis_workspace_revision.clone(),
            ),
            generation: outcome,
            changes,
            move_evidence,
            journal_seq_through: input.journal_seq,
            watcher_continuity: watch::continuity(self.connection())?,
        })
    }

    /// The confirmed-change path: advance the revision and publish.
    fn reconcile_changed(
        &self,
        workspace_root: &Path,
        config: &WorkspaceConfig,
        input: &InputSnapshot,
        observed: &[ObservedResource],
        changes: &[ResourceChange],
    ) -> Result<Option<GenerationRecord>, ScanError> {
        let (change_seq, revision) =
            next_workspace_revision(input.change_seq + 1, &input.workspace_revision);
        // The generation is begun against the revision this reconcile is
        // about to establish, and the clock is moved to that same value
        // inside the publication transaction -- so the basis and the
        // revision advance are one fact, committed together or not at all.
        let building = generation::begin_generation(self.connection(), &revision)?;

        let publication = Publication {
            generation_id: building.id,
            change_seq,
            revision,
        };
        match self.publish(
            &publication,
            workspace_root,
            config,
            input,
            observed,
            changes,
        ) {
            Ok(published) => Ok(Some(published)),
            Err(error) => {
                // The transaction has already rolled back by the time this
                // runs; the generation row predates it, so the abort is a
                // separate, deliberate write.
                generation::abort_generation(
                    self.connection(),
                    building.id,
                    &error.abort_reason(),
                )?;
                self.mark_recovery_required()?;
                Err(error)
            }
        }
    }

    /// The whole publication contract, in one transaction. Every early
    /// return drops `transaction`, which rolls back everything below.
    fn publish(
        &self,
        publication: &Publication,
        workspace_root: &Path,
        config: &WorkspaceConfig,
        input: &InputSnapshot,
        observed: &[ObservedResource],
        changes: &[ResourceChange],
    ) -> Result<GenerationRecord, ScanError> {
        let Publication {
            generation_id,
            change_seq,
            revision,
        } = publication;
        let generation_id = *generation_id;
        let transaction = self.resources.transaction()?;

        // Nothing moved under us, and the filesystem still looks the way
        // the plan assumed.
        self.verify_unchanged_input(&transaction, workspace_root, config, input, observed)?;

        // The confirmed input change, as a revision advance.
        generation::advance_change_seq(&transaction, *change_seq, revision)?;

        // Still BUILDING, and its basis is the revision just established.
        // The grant is what lets this transaction's Occurrences name the
        // generation it is about to publish (#16 task 13/14).
        let (building, grant) = generation::grant_publication(&transaction, generation_id)?;

        identity::apply_in_transaction(&self.resources, changes)?;
        scan::verify_resource_invariants(&self.resources, observed)?;

        // Structural recovery, for the Resources whose input actually
        // changed and no others: a bulk reconcile is not a reason to
        // re-parse a file nothing happened to (#16 task 14).
        let reanalyze: Vec<_> = changes
            .iter()
            .filter_map(changed_resource)
            .cloned()
            .collect();
        scan::publish_structure(&transaction, &grant, revision, workspace_root, &reanalyze)?;
        for change in changes {
            if let ResourceChange::Delete { id, .. } = change {
                // A tombstone has no structure to be current, last-valid,
                // or anything else.
                structural::clear(&transaction, *id)?;
            }
        }

        component::mark_current(&transaction, revision, generation_id)?;
        watch::mark_applied(&transaction, input.journal_seq, Some(generation_id))?;
        generation::record_reconcile(&transaction, input.journal_seq)?;
        let published = generation::finish_publish_stable(&transaction, &building)?;

        transaction.commit().map_err(ResourceError::from)?;
        Ok(published)
    }

    /// The no-op path: the filesystem and the persisted ACTIVE inventory
    /// agree, so no `ResourceId`, no `resource_revision`, no Workspace
    /// revision and no generation may move. What *is* written is
    /// bookkeeping: the outstanding candidates are resolved, the reconcile
    /// sequence is recorded, and `RESOURCE_INDEX` is restored to the state
    /// the verification just proved.
    fn reconcile_no_op(
        &self,
        workspace_root: &Path,
        config: &WorkspaceConfig,
        input: &InputSnapshot,
        observed: &[ObservedResource],
        changes: &[ResourceChange],
    ) -> Result<(), ScanError> {
        match self.commit_no_op(workspace_root, config, input, observed, changes) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.mark_recovery_required()?;
                Err(error)
            }
        }
    }

    fn commit_no_op(
        &self,
        workspace_root: &Path,
        config: &WorkspaceConfig,
        input: &InputSnapshot,
        observed: &[ObservedResource],
        changes: &[ResourceChange],
    ) -> Result<(), ScanError> {
        let transaction = self.resources.transaction()?;

        // Publishing nothing is still a claim that the index is current,
        // so it gets the same input verification a publication gets.
        self.verify_unchanged_input(&transaction, workspace_root, config, input, observed)?;

        // Only `MetadataRefresh`/`Unchanged` can be in here: size and mtime
        // are fast-path hints, not semantic input, so refreshing them
        // advances no revision and changes no fingerprint.
        identity::apply_in_transaction(&self.resources, changes)?;
        scan::verify_resource_invariants(&self.resources, observed)?;

        let stable =
            generation::current_stable(&transaction)?.ok_or(ScanError::InvariantViolated {
                detail: "a no-op reconcile requires an existing stable generation".to_owned(),
            })?;
        component::mark_current(&transaction, &input.workspace_revision, stable.id)?;
        // Resolved, with no generation to attribute them to: this reconcile
        // verified there was nothing for them to change.
        watch::mark_applied(&transaction, input.journal_seq, None)?;
        generation::record_reconcile(&transaction, input.journal_seq)?;

        transaction.commit().map_err(ResourceError::from)?;
        Ok(())
    }

    /// Everything the plan assumed is still true: the clock has not moved,
    /// the watcher has journalled nothing new, and a second full verified
    /// observation still matches the one that was planned against.
    fn verify_unchanged_input(
        &self,
        transaction: &Connection,
        workspace_root: &Path,
        config: &WorkspaceConfig,
        input: &InputSnapshot,
        observed: &[ObservedResource],
    ) -> Result<(), ScanError> {
        let current = generation::current_workspace_revision(transaction)?
            .ok_or(GenerationError::ClockNotBootstrapped)?;
        if current != input.workspace_revision {
            return Err(ScanError::InputChanged {
                detail: format!(
                    "workspace revision moved from {:?} to {current:?} during the reconcile",
                    input.workspace_revision
                ),
            });
        }
        if generation::change_seq(transaction)? != input.change_seq {
            return Err(ScanError::InputChanged {
                detail: "the input change sequence advanced during the reconcile".to_owned(),
            });
        }
        let journal_seq = watch::max_journal_seq(transaction)?;
        if journal_seq != input.journal_seq {
            return Err(ScanError::InputChanged {
                detail: format!(
                    "the watcher journalled candidate {journal_seq} during the reconcile"
                ),
            });
        }

        let fresh = scan::observe_workspace_verified(workspace_root, config)?;
        if let Some(detail) = scan::snapshot_drift(observed, &fresh) {
            return Err(ScanError::InputChanged { detail });
        }
        Ok(())
    }

    /// After a failed reconcile the Resource inventory is whatever it was
    /// before, which is exactly what could not be confirmed -- so it is
    /// left DIRTY/QUEUED and another reconcile is still required. An
    /// existing `last_error_code` is kept: failing to recover from a
    /// watcher continuity loss does not replace that loss as the reason.
    fn mark_recovery_required(&self) -> Result<(), ScanError> {
        let revision = self.workspace_revision()?;
        let unexplained =
            component::read(self.connection())?.is_none_or(|state| state.last_error_code.is_none());
        component::mark_dirty(
            self.connection(),
            &revision,
            unexplained.then_some(component::RECONCILE_FAILED_CODE),
        )?;
        Ok(())
    }

    fn connection(&self) -> &Connection {
        self.resources.connection()
    }
}

/// What the plan was made against, so the publication can prove none of it
/// moved before committing.
struct InputSnapshot {
    workspace_revision: String,
    change_seq: i64,
    journal_seq: i64,
}

/// The generation being published and the revision advance it carries --
/// one fact, so they travel together.
struct Publication {
    generation_id: i64,
    change_seq: i64,
    revision: String,
}

/// The Resource a planned decision re-analyzes, if it re-analyzes one.
///
/// A create or an update (which is also how an evidenced move arrives,
/// carrying its *new* path) changed the file's structural input. A
/// metadata refresh or an unchanged Resource did not, and re-parsing it
/// would be work with a known-identical answer.
fn changed_resource(change: &ResourceChange) -> Option<&crate::resource::Resource> {
    match change {
        ResourceChange::Create(resource) | ResourceChange::Update(resource) => Some(resource),
        ResourceChange::MetadataRefresh { .. }
        | ResourceChange::Unchanged { .. }
        | ResourceChange::Delete { .. } => None,
    }
}

/// Whether a planned decision is an actual input change -- the only kind
/// that may advance the Workspace revision.
///
/// `MetadataRefresh` is not one: size and mtime drifting while the
/// structural fingerprint holds is precisely the case #16 task 3 defines as
/// *not* a semantic change.
fn is_actual_change(change: &ResourceChange) -> bool {
    matches!(
        change,
        ResourceChange::Create(_) | ResourceChange::Update(_) | ResourceChange::Delete { .. }
    )
}

/// The next Workspace revision: the input-change sequence itself, as the
/// design requires ("monotonic input-change sequence", never a wall-clock
/// timestamp). The loop only matters for a Workspace bootstrapped with a
/// caller-chosen revision string that happens to read like the next
/// sequence number -- the revision must visibly differ from the one it
/// replaces.
fn next_workspace_revision(change_seq: i64, current: &str) -> (i64, String) {
    let mut change_seq = change_seq;
    loop {
        let candidate = change_seq.to_string();
        if candidate != current {
            return (change_seq, candidate);
        }
        change_seq += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
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
        resource::ResourceState,
        scan::BaselineScan,
        watch::{JournalState, RawWatchEvent, WatchEventKind, WatchIngest},
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    /// The Workspace revision the baseline bootstraps the clock with. It is
    /// deliberately not a number, so a test can see reconcile replace it
    /// with the input-change sequence.
    const BASELINE_REVISION: &str = "workspace-rev-1";

    /// A Workspace root plus an `index.db` outside it.
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-reconcile-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            fs::create_dir_all(&root).expect("workspace root should be creatable");
            Self { base, root }
        }

        fn db_path(&self) -> PathBuf {
            self.base.join("data").join("index.db")
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

        fn rename(&self, from: &str, to: &str) {
            fs::rename(self.root.join(from), self.root.join(to)).expect("rename should succeed");
        }

        /// An equal-length rewrite whose mtime is restored afterwards, so
        /// no metadata fast path can see it.
        fn overwrite_preserving_metadata(&self, rel: &str, contents: &str) {
            let path = self.root.join(rel);
            let before = fs::metadata(&path).expect("metadata");
            let mtime = before.modified().expect("mtime");
            assert_eq!(
                before.len(),
                contents.len() as u64,
                "this helper only makes sense for an equal-length rewrite"
            );
            fs::write(&path, contents).expect("rewrite");
            fs::File::options()
                .write(true)
                .open(&path)
                .expect("open")
                .set_modified(mtime)
                .expect("mtime should be restorable");
        }

        /// Publish the task 4 baseline, then close that connection.
        fn baseline(&self) -> i64 {
            let engine = BaselineScan::open(&self.db_path()).expect("index.db should open");
            engine
                .run_initial_scan(&self.root, &WorkspaceConfig::default(), BASELINE_REVISION)
                .expect("baseline scan should publish")
                .generation
                .id
        }

        /// Ingest watcher events, then close that connection.
        fn ingest(&self, events: &[RawWatchEvent]) {
            let ingest = WatchIngest::open(&self.db_path()).expect("index.db should open");
            ingest
                .ingest_all(&self.root, &WorkspaceConfig::default(), events)
                .expect("ingestion should succeed");
        }

        fn engine(&self) -> Reconcile {
            Reconcile::open(&self.db_path()).expect("index.db should open")
        }

        fn run(&self, engine: &Reconcile) -> Result<ReconcileReport, ScanError> {
            engine.run(&self.root, &WorkspaceConfig::default())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// Every ACTIVE Resource as `path_rel → (id, revision)`.
    fn inventory(engine: &Reconcile) -> BTreeMap<String, (ResourceId, String)> {
        engine
            .resources()
            .list_active()
            .expect("list should succeed")
            .into_iter()
            .map(|resource| (resource.path_rel, (resource.id, resource.resource_revision)))
            .collect()
    }

    fn index_state(engine: &Reconcile) -> ResourceIndexState {
        engine
            .resource_index_state()
            .expect("component read ok")
            .expect("the baseline published a component row")
    }

    fn assert_ready_and_current(engine: &Reconcile) {
        let state = index_state(engine);
        assert_eq!(state.processing_state, ProcessingState::Ready);
        assert_eq!(state.freshness_state, FreshnessState::Current);
    }

    #[test]
    fn a_missed_create_is_recovered_from_the_filesystem_with_no_journal_at_all() {
        let fixture = Fixture::create("missed-create");
        fixture.write("lib.rs", "fn main() {}");
        fixture.baseline();
        // No watcher, no event, no journal row: the file simply appears.
        fixture.write("extra.rs", "fn extra() {}");

        let engine = fixture.engine();
        let report = fixture.run(&engine).expect("reconcile should succeed");

        assert!(report.published(), "a missed create is an actual change");
        assert!(engine.journal().expect("journal ok").is_empty());
        assert!(inventory(&engine).contains_key("extra.rs"));
        assert_ready_and_current(&engine);
    }

    #[test]
    fn a_missed_delete_is_recovered_from_the_filesystem_with_no_journal_at_all() {
        let fixture = Fixture::create("missed-delete");
        fixture.write("lib.rs", "fn main() {}");
        fixture.write("gone.rs", "fn gone() {}");
        fixture.baseline();
        let engine = fixture.engine();
        let before = inventory(&engine);
        fixture.remove("gone.rs");

        let report = fixture.run(&engine).expect("reconcile should succeed");

        assert!(report.published());
        let after = inventory(&engine);
        assert!(!after.contains_key("gone.rs"), "the deletion must be seen");
        assert_eq!(
            after["lib.rs"], before["lib.rs"],
            "an untouched Resource keeps its id and revision"
        );
        let tombstone = engine
            .resources()
            .get_by_id(before["gone.rs"].0)
            .expect("get ok")
            .expect("the tombstone keeps the identity");
        assert_eq!(tombstone.state, ResourceState::Deleted);
        assert_eq!(tombstone.resource_revision, "2");
    }

    #[test]
    fn a_missed_modify_is_recovered_from_the_filesystem_with_no_journal_at_all() {
        let fixture = Fixture::create("missed-modify");
        fixture.write("lib.rs", "fn main() {}");
        fixture.baseline();
        let engine = fixture.engine();
        let before = inventory(&engine);
        fixture.write("lib.rs", "fn main() { let recovered = 1; }");

        let report = fixture.run(&engine).expect("reconcile should succeed");

        assert!(report.published());
        let after = inventory(&engine);
        assert_eq!(after["lib.rs"].0, before["lib.rs"].0, "identity survives");
        assert_eq!(after["lib.rs"].1, "2", "content change advances revision");
    }

    #[test]
    fn a_same_size_same_mtime_modify_is_recovered_because_reconcile_is_verified() {
        let fixture = Fixture::create("stealth-modify");
        fixture.write("lib.rs", "fn main() { a() }");
        fixture.baseline();
        let engine = fixture.engine();
        let before = inventory(&engine);

        // Invisible to any metadata fast path, by construction.
        fixture.overwrite_preserving_metadata("lib.rs", "fn main() { b() }");

        let report = fixture.run(&engine).expect("reconcile should succeed");

        assert!(
            report.published(),
            "the correctness path must hash current bytes, not trust metadata"
        );
        let after = inventory(&engine);
        assert_eq!(after["lib.rs"].0, before["lib.rs"].0);
        assert_eq!(after["lib.rs"].1, "2");
    }

    #[test]
    fn a_bulk_change_after_a_continuity_loss_is_recovered_and_continuity_stays_lost() {
        let fixture = Fixture::create("continuity-lost");
        fixture.write("lib.rs", "fn main() {}");
        fixture.write("old.rs", "fn old() {}");
        fixture.baseline();

        // The watcher gives up; everything after this is unobserved.
        fixture.ingest(&[RawWatchEvent::ContinuityLost {
            detail: "events dropped".to_owned(),
        }]);
        fixture.write("lib.rs", "fn main() { changed() }");
        fixture.write("new.rs", "fn new() {}");
        fixture.remove("old.rs");

        let engine = fixture.engine();
        let dirty = index_state(&engine);
        assert_eq!(dirty.freshness_state, FreshnessState::Dirty);
        let report = fixture.run(&engine).expect("reconcile should succeed");

        assert!(report.published());
        let after = inventory(&engine);
        assert!(after.contains_key("new.rs"));
        assert!(!after.contains_key("old.rs"));
        assert_eq!(after["lib.rs"].1, "2");
        assert_ready_and_current(&engine);
        assert_eq!(
            report.watcher_continuity,
            WatcherContinuity::Lost,
            "recovering the index does not repair the event stream"
        );
    }

    #[test]
    fn an_explicit_paired_rename_keeps_the_resource_identity() {
        let fixture = Fixture::create("paired-rename");
        fixture.write("before.rs", "fn moved() {}");
        fixture.baseline();
        let engine = fixture.engine();
        let before = inventory(&engine);

        fixture.rename("before.rs", "after.rs");
        fixture.ingest(&[RawWatchEvent::RenamedPair {
            from: fixture.root.join("before.rs"),
            to: fixture.root.join("after.rs"),
        }]);

        let report = fixture.run(&engine).expect("reconcile should succeed");

        assert_eq!(report.move_evidence.len(), 1);
        let after = inventory(&engine);
        assert!(!after.contains_key("before.rs"));
        assert_eq!(
            after["after.rs"].0, before["before.rs"].0,
            "vouched-for move evidence keeps the id"
        );
        assert_eq!(after["after.rs"].1, "2", "the path change is a change");
    }

    #[test]
    fn a_half_rename_is_resolved_as_delete_plus_create_not_as_a_move() {
        let fixture = Fixture::create("half-rename");
        fixture.write("before.rs", "fn moved() {}");
        fixture.baseline();
        let engine = fixture.engine();
        let before = inventory(&engine);

        // The backend only saw the two halves, so that is all it reports --
        // identical content is not evidence they are the same Resource.
        fixture.rename("before.rs", "after.rs");
        fixture.ingest(&[
            RawWatchEvent::Removed {
                path: fixture.root.join("before.rs"),
            },
            RawWatchEvent::Created {
                path: fixture.root.join("after.rs"),
            },
        ]);

        let report = fixture.run(&engine).expect("reconcile should succeed");

        assert!(report.move_evidence.is_empty());
        let after = inventory(&engine);
        assert_ne!(
            after["after.rs"].0, before["before.rs"].0,
            "no identity may be carried across an unproven rename"
        );
        assert_eq!(
            after["after.rs"].1, "1",
            "the destination is a new Resource"
        );
        assert_eq!(
            engine
                .resources()
                .get_by_id(before["before.rs"].0)
                .expect("get ok")
                .expect("the source keeps its identity as a tombstone")
                .state,
            ResourceState::Deleted
        );
    }

    #[test]
    fn an_ambiguous_rename_cycle_produces_no_false_merge() {
        let fixture = Fixture::create("rename-cycle");
        fixture.write("left.rs", "fn left() {}");
        fixture.write("right.rs", "fn right() { swapped() }");
        fixture.baseline();
        let engine = fixture.engine();
        let before = inventory(&engine);

        // A↔B swap: every endpoint is both a source and a destination.
        fixture.rename("left.rs", "swap.tmp");
        fixture.rename("right.rs", "left.rs");
        fixture.rename("swap.tmp", "right.rs");
        fixture.ingest(&[
            RawWatchEvent::RenamedPair {
                from: fixture.root.join("left.rs"),
                to: fixture.root.join("right.rs"),
            },
            RawWatchEvent::RenamedPair {
                from: fixture.root.join("right.rs"),
                to: fixture.root.join("left.rs"),
            },
        ]);

        let report = fixture.run(&engine).expect("reconcile should succeed");

        assert!(
            report.move_evidence.is_empty(),
            "a cycle is not confirmable evidence"
        );
        let after = inventory(&engine);
        assert_eq!(
            after["left.rs"].0, before["left.rs"].0,
            "the occupied path keeps its own identity rather than merging"
        );
        assert_eq!(after["right.rs"].0, before["right.rs"].0);
        assert_eq!(after["left.rs"].1, "2", "its content did change");
        assert_eq!(after["right.rs"].1, "2");
        assert_eq!(
            engine.journal().expect("journal ok").len(),
            2,
            "both rename rows stay as evidence"
        );
    }

    #[test]
    fn a_no_op_reconcile_advances_no_revision_and_publishes_no_generation() {
        let fixture = Fixture::create("no-op");
        fixture.write("lib.rs", "fn main() {}");
        let baseline = fixture.baseline();
        fixture.ingest(&[RawWatchEvent::Modified {
            path: fixture.root.join("lib.rs"),
        }]);

        let engine = fixture.engine();
        let before = inventory(&engine);
        let report = fixture.run(&engine).expect("reconcile should succeed");

        assert!(!report.published(), "nothing changed, so nothing publishes");
        assert_eq!(report.workspace_revision, BASELINE_REVISION);
        assert_eq!(
            engine.workspace_revision().expect("revision ok"),
            BASELINE_REVISION
        );
        assert_eq!(inventory(&engine), before, "no Resource revision moves");
        assert_eq!(
            engine
                .current_stable()
                .expect("current ok")
                .expect("stable exists")
                .id,
            baseline,
            "the baseline generation stays current"
        );
        // The candidate is still resolved, and the bookkeeping recorded it.
        let journal = engine.journal().expect("journal ok");
        assert_eq!(journal[0].processing_state, JournalState::Applied);
        assert_eq!(journal[0].applied_generation_id, None);
        assert_eq!(engine.last_reconcile_seq().expect("seq ok"), journal[0].seq);
        assert_ready_and_current(&engine);
    }

    #[test]
    fn a_successful_reconcile_links_the_journal_to_the_generation_that_applied_it() {
        let fixture = Fixture::create("journal-link");
        fixture.write("lib.rs", "fn main() {}");
        fixture.baseline();
        fixture.write("lib.rs", "fn main() { edited() }");
        // Two rapid MODIFYs, so one row coalesces into the other.
        let event = RawWatchEvent::Modified {
            path: fixture.root.join("lib.rs"),
        };
        fixture.ingest(&[event.clone(), event]);

        let engine = fixture.engine();
        let report = fixture.run(&engine).expect("reconcile should succeed");
        let generation = report.generation.clone().expect("a change was published");

        assert_eq!(generation.state, GenerationState::Stable);
        let journal = engine.journal().expect("journal ok");
        assert_eq!(journal.len(), 2, "no journal row is ever deleted");
        assert_eq!(
            journal[0].processing_state,
            JournalState::Coalesced,
            "superseded evidence is preserved as it was"
        );
        assert_eq!(journal[0].coalesced_into_seq, Some(journal[1].seq));
        assert_eq!(journal[0].applied_generation_id, None);
        assert_eq!(journal[1].processing_state, JournalState::Applied);
        assert_eq!(journal[1].applied_generation_id, Some(generation.id));
        assert_eq!(engine.last_reconcile_seq().expect("seq ok"), journal[1].seq);
        assert_eq!(report.journal_seq_through, journal[1].seq);
    }

    #[test]
    fn a_confirmed_change_advances_the_workspace_revision_as_the_input_change_sequence() {
        let fixture = Fixture::create("revision-advance");
        fixture.write("lib.rs", "fn main() {}");
        fixture.baseline();
        let engine = fixture.engine();

        fixture.write("lib.rs", "fn main() { one() }");
        let first = fixture.run(&engine).expect("reconcile should succeed");
        fixture.write("lib.rs", "fn main() { two() }");
        let second = fixture.run(&engine).expect("reconcile should succeed");

        assert_eq!(first.workspace_revision, "1");
        assert_eq!(second.workspace_revision, "2");
        for report in [&first, &second] {
            let generation = report.generation.as_ref().expect("published");
            assert_eq!(
                generation.basis_workspace_revision, report.workspace_revision,
                "the generation basis is the revision the advance established"
            );
        }
        assert_eq!(
            index_state(&engine).basis_workspace_revision,
            second.workspace_revision
        );

        // A third reconcile with nothing to find must not move it again.
        let third = fixture.run(&engine).expect("reconcile should succeed");
        assert!(!third.published());
        assert_eq!(third.workspace_revision, "2");
    }

    #[test]
    fn a_filesystem_change_during_the_reconcile_keeps_the_previous_stable_generation() {
        let fixture = Fixture::create("mid-flight-drift");
        fixture.write("lib.rs", "fn main() {}");
        let baseline = fixture.baseline();
        let engine = fixture.engine();
        let before = inventory(&engine);

        // A plan made against a snapshot that the filesystem then leaves
        // behind -- the shape of any edit landing mid-reconcile.
        let (input, stale, changes) = plan_against_current(&fixture, &engine);
        fixture.write("lib.rs", "fn main() { raced() }");
        let error = engine
            .reconcile_changed(
                &fixture.root,
                &WorkspaceConfig::default(),
                &input,
                &stale,
                &changes,
            )
            .expect_err("a mid-reconcile change must not be published as current");

        assert!(matches!(error, ScanError::InputChanged { .. }));
        assert!(error.is_retryable());
        assert_eq!(inventory(&engine), before, "nothing was applied");
        assert_eq!(
            engine
                .current_stable()
                .expect("current ok")
                .expect("stable exists")
                .id,
            baseline,
            "the previous stable generation survives"
        );
        assert_eq!(
            engine.workspace_revision().expect("revision ok"),
            BASELINE_REVISION,
            "an aborted reconcile advances no revision"
        );
    }

    #[test]
    fn a_failed_publication_rolls_back_and_leaves_the_index_dirty() {
        let fixture = Fixture::create("failed-publish");
        fixture.write("lib.rs", "fn main() {}");
        fixture.baseline();
        fixture.ingest(&[RawWatchEvent::Modified {
            path: fixture.root.join("lib.rs"),
        }]);
        let engine = fixture.engine();

        let (input, stale, changes) = plan_against_current(&fixture, &engine);
        fixture.write("lib.rs", "fn main() { raced() }");
        engine
            .reconcile_changed(
                &fixture.root,
                &WorkspaceConfig::default(),
                &input,
                &stale,
                &changes,
            )
            .expect_err("the publication must fail");

        let state = index_state(&engine);
        assert_eq!(state.processing_state, ProcessingState::Queued);
        assert_eq!(
            state.freshness_state,
            FreshnessState::Dirty,
            "a failed reconcile never claims the index is current"
        );
        assert!(state.stable_generation_id.is_some(), "last valid snapshot");
        assert_eq!(
            engine.journal().expect("journal ok")[0].processing_state,
            JournalState::Pending,
            "a failed reconcile does not mark candidates handled"
        );
        let building = engine
            .journal()
            .expect("journal ok")
            .iter()
            .filter_map(|entry| entry.applied_generation_id)
            .count();
        assert_eq!(building, 0);
    }

    #[test]
    fn a_candidate_journalled_during_the_reconcile_blocks_the_publication() {
        let fixture = Fixture::create("journal-race");
        fixture.write("lib.rs", "fn main() {}");
        fixture.baseline();
        let engine = fixture.engine();

        let (input, observed, changes) = plan_against_current(&fixture, &engine);
        // The watcher reports something after the plan was made. The
        // filesystem still matches, so only the journal guard can catch it.
        fixture.ingest(&[RawWatchEvent::Modified {
            path: fixture.root.join("lib.rs"),
        }]);
        let error = engine
            .reconcile_changed(
                &fixture.root,
                &WorkspaceConfig::default(),
                &input,
                &observed,
                &changes,
            )
            .expect_err("a newer candidate must block the publication");

        assert!(matches!(error, ScanError::InputChanged { .. }));
        assert_eq!(
            engine.journal().expect("journal ok")[0].processing_state,
            JournalState::Pending
        );
    }

    #[test]
    fn a_reconcile_after_reopen_recovers_correctness() {
        let fixture = Fixture::create("reopen");
        fixture.write("lib.rs", "fn main() {}");
        fixture.baseline();
        // Everything below happens while no engine holds the database.
        fixture.write("lib.rs", "fn main() { offline() }");
        fixture.write("added.rs", "fn added() {}");

        let engine = fixture.engine();
        let report = fixture.run(&engine).expect("reconcile should succeed");

        assert!(report.published());
        let after = inventory(&engine);
        assert!(after.contains_key("added.rs"));
        assert_eq!(after["lib.rs"].1, "2");

        // And the recovered state survives another reopen.
        drop(engine);
        let reopened = fixture.engine();
        assert_eq!(inventory(&reopened), after);
        assert_ready_and_current(&reopened);
    }

    #[test]
    fn two_worktrees_reconcile_independently() {
        let left = Fixture::create("worktree-left");
        let right = Fixture::create("worktree-right");
        for fixture in [&left, &right] {
            fixture.write("lib.rs", "fn main() {}");
            fixture.baseline();
        }
        let right_engine = right.engine();
        let right_before = inventory(&right_engine);

        left.write("only-left.rs", "fn left() {}");
        let left_engine = left.engine();
        assert!(left.run(&left_engine).expect("reconcile ok").published());

        assert!(inventory(&left_engine).contains_key("only-left.rs"));
        assert!(!inventory(&right_engine).contains_key("only-left.rs"));
        let right_report = right.run(&right_engine).expect("reconcile ok");
        assert!(!right_report.published(), "the other worktree is unchanged");
        assert_eq!(inventory(&right_engine), right_before);
        assert_eq!(right_report.workspace_revision, BASELINE_REVISION);
    }

    #[test]
    fn only_paired_renames_become_move_evidence() {
        let fixture = Fixture::create("evidence-kinds");
        fixture.write("lib.rs", "fn main() {}");
        fixture.baseline();
        fixture.ingest(&[
            RawWatchEvent::Removed {
                path: fixture.root.join("lib.rs"),
            },
            RawWatchEvent::Created {
                path: fixture.root.join("lib.rs"),
            },
        ]);

        let engine = fixture.engine();
        let kinds: Vec<WatchEventKind> = engine
            .journal()
            .expect("journal ok")
            .iter()
            .map(|entry| entry.event_kind)
            .collect();

        assert_eq!(
            kinds,
            vec![WatchEventKind::Delete, WatchEventKind::Create],
            "a delete plus a create is never folded into a move"
        );
        assert!(
            watch::pending_move_evidence(engine.connection())
                .expect("evidence ok")
                .is_empty()
        );
    }

    /// The planning half of [`Reconcile::run`], so a test can hold a plan
    /// still and change the world underneath it.
    fn plan_against_current(
        fixture: &Fixture,
        engine: &Reconcile,
    ) -> (InputSnapshot, Vec<ObservedResource>, Vec<ResourceChange>) {
        let config = WorkspaceConfig::default();
        let input = InputSnapshot {
            workspace_revision: engine.workspace_revision().expect("revision ok"),
            change_seq: generation::change_seq(engine.connection()).expect("seq ok"),
            journal_seq: watch::max_journal_seq(engine.connection()).expect("journal seq ok"),
        };
        let observed =
            scan::observe_workspace_verified(&fixture.root, &config).expect("observation ok");
        let active = engine.resources().list_active().expect("list ok");
        let changes = identity::plan_changes(&active, &observed, &[]).expect("plan ok");
        (input, observed, changes)
    }
}
