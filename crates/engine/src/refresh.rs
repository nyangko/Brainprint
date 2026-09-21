//! The targeted structural refresh: one saved file becomes one published
//! generation, without re-walking the Workspace (#16 task 13).
//!
//! Reconcile (#16 task 6) is correct because it looks at everything. That
//! is also why it is the wrong thing to run every time somebody presses
//! save. This path handles the overwhelmingly common case -- an existing
//! ACTIVE source file was modified, and nothing else happened -- by
//! verifying, parsing, extracting, and publishing that one Resource.
//!
//! ## Flow
//!
//! `pending candidates → eligibility → Verified observation of the one
//! target → parse/extract its current bytes → [ one transaction: clock +
//! journal + target re-verification → revision advance → publication grant
//! → Resource update → Symbol continuity + replacement → Occurrence
//! replacement → journal APPLIED → RESOURCE_INDEX CURRENT → STABLE +
//! stable pointer swap ] → commit`.
//!
//! ## Eligibility is a refusal, not a guess
//!
//! The fast path exists for one shape: a single MODIFY of a single
//! existing ACTIVE Resource whose classification did not change. A move, a
//! delete-then-create, a bulk hint, a continuity loss, two independent
//! pending Resources, a path that is now excluded -- every one of those is
//! [`RefreshOutcome::Deferred`], and reconcile deals with it. Guessing that
//! two events are "the same file, probably" is how a stable id ends up on
//! the wrong content; deferring costs a full reconcile and nothing else.
//!
//! The same reasoning covers the whole Workspace claim: if anything
//! pending is *not* about the target, this path does not publish, because
//! marking `RESOURCE_INDEX` CURRENT would then assert something about
//! changes nobody has looked at yet.
//!
//! ## No full scan
//!
//! Nothing here calls [`discovery::enumerate_resources`] or
//! [`scan::observe_workspace_verified`]. One path is classified
//! ([`discovery::describe_path`]), one file is hashed, one file is parsed.
//! No other Resource is read, hashed, or re-parsed -- which is the whole
//! point of the path and is asserted directly in its tests.
//!
//! ## What is out of scope
//!
//! A `PARTIAL` parse or a container-only dialect stops the publication
//! *before* anything is replaced, so the previously accepted Symbols and
//! Occurrences survive untouched and the component stays DIRTY. Deciding
//! what a partial result may still be published as is #16 task 14's
//! last-valid policy, not this task's. Incremental reparse is likewise not
//! built here: a fresh parse of one file is correct, and an edit-tracking
//! cache is an architecture that needs a benchmark before it needs code.

use std::{fs, path::Path};

use brainprint_core::ResourceId;
use rusqlite::Connection;

use crate::{
    component,
    config::WorkspaceConfig,
    discovery::{self, DiscoveredResource},
    extract::{self, Extraction},
    generation::{self, GenerationError, GenerationRecord},
    identity::{self, ObservationMode, ObservedResource, ResourceChange},
    parser::{self, ParseError, ParseStatus, ParserRegistry, SourceBasis},
    resource::{Resource, ResourceError, ResourceStore},
    scan::{self, ScanError},
    schema, symbol,
    watch::{self, JournalEntry, WatchEventKind, WatcherContinuity},
};

/// What a targeted refresh attempt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// The target's new structure was published as a new STABLE
    /// generation.
    Published(RefreshPublication),
    /// The file's verified content is identical to what is indexed. No
    /// revision moved and no generation was created; the candidate was
    /// simply resolved.
    NoOp {
        resource_id: ResourceId,
        path_rel: String,
    },
    /// The fast path does not cover this situation. Nothing was written,
    /// the journal keeps its candidates, and reconcile is required.
    Deferred(DeferReason),
    /// The Workspace moved while the refresh was running, so the analysis
    /// describes source that is already out of date. Nothing is published,
    /// the candidates stay PENDING, `RESOURCE_INDEX` stays DIRTY -- and
    /// nothing is retried automatically.
    Obsolete(ObsoleteReason),
}

impl RefreshOutcome {
    #[must_use]
    pub const fn published(&self) -> Option<&RefreshPublication> {
        match self {
            Self::Published(publication) => Some(publication),
            _ => None,
        }
    }
}

/// What a successful targeted publication established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshPublication {
    pub resource_id: ResourceId,
    pub path_rel: String,
    /// The Resource's new `resource_revision`.
    pub resource_revision: String,
    /// The Workspace revision this publication established.
    pub workspace_revision: String,
    pub generation: GenerationRecord,
    pub symbols: usize,
    pub occurrences: usize,
}

/// Why the fast path handed the situation to reconcile. Each variant is a
/// case where a targeted publication could only proceed by guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeferReason {
    /// No outstanding candidate: there is nothing to refresh.
    NothingPending,
    /// The watcher lost events, so the pending set is not the change set.
    WatcherContinuityLost,
    /// A candidate that is not a plain modify of an existing Resource.
    UnsupportedEvent { kind: WatchEventKind },
    /// A candidate whose path no ACTIVE Resource occupies -- a create, or
    /// a delete-then-create whose identity is exactly what must not be
    /// guessed.
    NotAnExistingResource { path: Option<String> },
    /// More than one Resource has outstanding candidates. Publishing one
    /// of them cannot make the Workspace current.
    MultiplePendingResources { resources: usize },
    /// The target is no longer a file on disk.
    TargetPathMissing { path_rel: String },
    /// The target's path is now excluded by the discovery rules.
    TargetExcluded { path_rel: String },
    /// Kind, role, or language changed: this is no longer the same kind of
    /// Resource, and its structural treatment may differ.
    ClassificationChanged { path_rel: String },
    /// No Tier-1 structural dialect covers the target, so there is no
    /// structural result to publish. The Resource change itself is left
    /// for reconcile rather than published half-analyzed.
    NotStructuralSource { path_rel: String },
    /// The parse did not accept the whole file (an incomplete edit, or a
    /// container-only dialect). The previously accepted structure is kept
    /// exactly as it was.
    ExtractionNotAccepted {
        path_rel: String,
        parse_status: ParseStatus,
    },
    /// The single-Resource comparison produced something other than a
    /// modification of the same identity.
    IdentityNotContinuous { path_rel: String },
}

/// Why an in-flight refresh was discarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObsoleteReason {
    /// The target file changed again after it was read.
    TargetChangedDuringRefresh { path_rel: String, detail: String },
    /// A new candidate was journalled during the refresh.
    JournalAdvanced { from_seq: i64, to_seq: i64 },
    /// The Workspace revision or input-change sequence moved.
    ClockAdvanced { detail: String },
}

/// Failure of a targeted refresh. A failure is not a defer: the fast path
/// was eligible and something went wrong, so the publication is rolled
/// back, the generation is ABORTED, and `RESOURCE_INDEX` is left DIRTY.
#[derive(Debug)]
pub enum RefreshError {
    Scan(ScanError),
    Symbol(symbol::SymbolError),
    Parse(ParseError),
    /// The Resource row did not describe the published bytes after the
    /// update was applied. A bug, not a race: rolled back.
    InvariantViolated {
        detail: String,
    },
}

impl RefreshError {
    fn abort_reason(&self) -> String {
        format!("targeted refresh failed: {self}")
    }
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Scan(source) => write!(formatter, "{source}"),
            Self::Symbol(source) => write!(formatter, "structural write failed: {source}"),
            Self::Parse(source) => write!(formatter, "parse failed: {source}"),
            Self::InvariantViolated { detail } => write!(
                formatter,
                "resource invariant violated, so the targeted publication was rolled back: \
                 {detail}"
            ),
        }
    }
}

impl std::error::Error for RefreshError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Scan(source) => Some(source),
            Self::Symbol(source) => Some(source),
            Self::Parse(source) => Some(source),
            Self::InvariantViolated { .. } => None,
        }
    }
}

impl From<ScanError> for RefreshError {
    fn from(source: ScanError) -> Self {
        Self::Scan(source)
    }
}

impl From<symbol::SymbolError> for RefreshError {
    fn from(source: symbol::SymbolError) -> Self {
        Self::Symbol(source)
    }
}

impl From<ParseError> for RefreshError {
    fn from(source: ParseError) -> Self {
        Self::Parse(source)
    }
}

impl From<ResourceError> for RefreshError {
    fn from(source: ResourceError) -> Self {
        Self::Scan(ScanError::from(source))
    }
}

impl From<GenerationError> for RefreshError {
    fn from(source: GenerationError) -> Self {
        Self::Scan(ScanError::from(source))
    }
}

impl From<identity::IdentityError> for RefreshError {
    fn from(source: identity::IdentityError) -> Self {
        Self::Scan(ScanError::from(source))
    }
}

impl From<watch::WatchError> for RefreshError {
    fn from(source: watch::WatchError) -> Self {
        Self::Scan(ScanError::from(source))
    }
}

impl From<component::ComponentError> for RefreshError {
    fn from(source: component::ComponentError) -> Self {
        Self::Scan(ScanError::from(source))
    }
}

impl From<discovery::DiscoveryError> for RefreshError {
    fn from(source: discovery::DiscoveryError) -> Self {
        Self::Scan(ScanError::from(source))
    }
}

/// One Workspace's targeted refresh path, bound to one `index.db`
/// connection: the Resource row, the Symbols, the Occurrences, the
/// journal, the clock and the generation it publishes all have to be
/// written through the same connection to share one transaction.
pub struct TargetedRefresh {
    resources: ResourceStore,
}

impl TargetedRefresh {
    /// Open (creating/migrating if needed) the `index.db` at `path`.
    pub fn open(path: &Path) -> Result<Self, RefreshError> {
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

    #[must_use]
    pub fn resources(&self) -> &ResourceStore {
        &self.resources
    }

    /// Try to bring the structural index up to date for one saved file.
    ///
    /// Returns without writing anything whenever the situation is not the
    /// single-file modify this path covers.
    pub fn run(
        &self,
        workspace_root: &Path,
        config: &WorkspaceConfig,
    ) -> Result<RefreshOutcome, RefreshError> {
        let input = self.snapshot_input()?;

        let target = match self.select_target(workspace_root, config)? {
            Err(reason) => return Ok(RefreshOutcome::Deferred(reason)),
            Ok(target) => target,
        };

        // The one file, hashed for real -- and then read once more so the
        // bytes that are parsed are provably the bytes that were verified.
        let observed = identity::observe(
            workspace_root,
            &target.discovered,
            Some(&target.resource),
            ObservationMode::Verified,
        )?;
        let path = workspace_root.join(&target.resource.path_rel);
        let bytes = fs::read(&path).map_err(|source| {
            RefreshError::from(identity::IdentityError::Io {
                path: path.clone(),
                source,
            })
        })?;
        if observed.content_hash.as_deref() != Some(identity::content_hash_of(&bytes).as_str()) {
            return Ok(RefreshOutcome::Obsolete(
                ObsoleteReason::TargetChangedDuringRefresh {
                    path_rel: target.resource.path_rel.clone(),
                    detail: "the file changed between hashing and reading it".to_owned(),
                },
            ));
        }

        // A one-Resource comparison: the same identity rules as reconcile,
        // over a slice of one instead of the whole Workspace.
        let changes = identity::plan_changes(
            std::slice::from_ref(&target.resource),
            std::slice::from_ref(&observed),
            &[],
        )?;
        match changes.as_slice() {
            [ResourceChange::Unchanged { .. }] | [ResourceChange::MetadataRefresh { .. }] => {
                self.finish_no_op(&input, &target, &changes)
            }
            [ResourceChange::Update(updated)] => {
                let updated = updated.clone();
                self.refresh_changed(
                    workspace_root,
                    config,
                    &input,
                    &target,
                    &updated,
                    &observed,
                    &bytes,
                )
            }
            _ => Ok(RefreshOutcome::Deferred(
                DeferReason::IdentityNotContinuous {
                    path_rel: target.resource.path_rel.clone(),
                },
            )),
        }
    }

    /// The single eligible target, or the reason there is none.
    ///
    /// Reads the journal and one path. Deliberately no Workspace walk: the
    /// pending candidates say which Resource to look at, and everything
    /// else about the situation is a reason to defer.
    fn select_target(
        &self,
        workspace_root: &Path,
        config: &WorkspaceConfig,
    ) -> Result<Selected, RefreshError> {
        if watch::continuity(self.connection())? == WatcherContinuity::Lost {
            return Ok(Err(DeferReason::WatcherContinuityLost));
        }
        let pending = watch::pending_candidates(self.connection())?;
        if pending.is_empty() {
            return Ok(Err(DeferReason::NothingPending));
        }
        if let Some(reason) = unsupported_candidate(&pending) {
            return Ok(Err(reason));
        }

        let mut ids: Vec<ResourceId> = pending
            .iter()
            .filter_map(|entry| entry.resource_id)
            .collect();
        ids.sort_by_key(|id| id.to_bytes());
        ids.dedup();
        let [resource_id] = ids.as_slice() else {
            return Ok(Err(DeferReason::MultiplePendingResources {
                resources: ids.len(),
            }));
        };

        let Some(resource) = self.resources.get_by_id(*resource_id)? else {
            return Ok(Err(DeferReason::NotAnExistingResource { path: None }));
        };
        if resource.state != crate::resource::ResourceState::Active {
            return Ok(Err(DeferReason::NotAnExistingResource {
                path: Some(resource.path_rel),
            }));
        }

        // One path classified, not a Workspace enumerated.
        if discovery::is_ignored_path(workspace_root, &resource.path_rel, config) {
            return Ok(Err(DeferReason::TargetExcluded {
                path_rel: resource.path_rel,
            }));
        }
        let Some(discovered) = discovery::describe_path(workspace_root, &resource.path_rel)? else {
            return Ok(Err(DeferReason::TargetPathMissing {
                path_rel: resource.path_rel,
            }));
        };
        if discovered.kind != resource.kind
            || discovered.role != resource.role
            || discovered.language != resource.language
        {
            return Ok(Err(DeferReason::ClassificationChanged {
                path_rel: resource.path_rel,
            }));
        }

        Ok(Ok(Target {
            resource,
            discovered,
        }))
    }

    /// The confirmed-change path: parse, extract, then publish it all in
    /// one transaction.
    #[allow(clippy::too_many_arguments)]
    fn refresh_changed(
        &self,
        workspace_root: &Path,
        config: &WorkspaceConfig,
        input: &InputSnapshot,
        target: &Target,
        updated: &Resource,
        observed: &ObservedResource,
        bytes: &[u8],
    ) -> Result<RefreshOutcome, RefreshError> {
        // The structural result comes from the verified bytes, and only a
        // whole-file COMPLETE parse may replace anything.
        let dialect = match parser::dialect_for_resource(updated) {
            Ok(dialect) => dialect,
            Err(_) => {
                return Ok(RefreshOutcome::Deferred(DeferReason::NotStructuralSource {
                    path_rel: updated.path_rel.clone(),
                }));
            }
        };
        // ponytail: a fresh parse of one file. An incremental reparse
        // needs a retained tree plus an exact SourceEdit, and neither
        // exists to be trusted here -- building that cache is its own
        // task, with its own benchmark.
        let tree = ParserRegistry::new().parse(dialect, bytes, SourceBasis::of(updated))?;
        let extraction = extract::extract(&tree, bytes);
        if !extraction.is_accepted() {
            // Nothing is replaced and nothing is deleted: the previously
            // accepted Symbols and Occurrences stay exactly as they are.
            return Ok(RefreshOutcome::Deferred(
                DeferReason::ExtractionNotAccepted {
                    path_rel: updated.path_rel.clone(),
                    parse_status: tree.status(),
                },
            ));
        }

        let (change_seq, revision) =
            next_workspace_revision(input.change_seq + 1, &input.workspace_revision);
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
            target,
            updated,
            observed,
            &extraction,
        ) {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                // The transaction has already rolled back; the generation
                // row predates it, so aborting it is a separate write.
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

    /// The whole targeted publication contract, in one transaction. Every
    /// early return drops `transaction`, which rolls back all of it.
    #[allow(clippy::too_many_arguments)]
    fn publish(
        &self,
        publication: &Publication,
        workspace_root: &Path,
        config: &WorkspaceConfig,
        input: &InputSnapshot,
        target: &Target,
        updated: &Resource,
        observed: &ObservedResource,
        extraction: &Extraction,
    ) -> Result<RefreshOutcome, RefreshError> {
        let transaction = self.resources.transaction()?;

        // 1. Nothing moved: not the clock, not the journal, not the file.
        if let Some(reason) = verify_unchanged(
            &transaction,
            workspace_root,
            config,
            input,
            target,
            observed,
        )? {
            drop(transaction);
            generation::abort_generation(
                self.connection(),
                publication.generation_id,
                &obsolete_reason_text(&reason),
            )?;
            return Ok(RefreshOutcome::Obsolete(reason));
        }

        // 2. The confirmed input change, as a revision advance.
        generation::advance_change_seq(
            &transaction,
            publication.change_seq,
            &publication.revision,
        )?;

        // 3. Still BUILDING, basis still current -- and the token that
        //    lets this transaction's Occurrences name it.
        let (building, grant) =
            generation::grant_publication(&transaction, publication.generation_id)?;

        // 4. The Resource itself.
        self.resources.update_resource(updated)?;
        verify_target_row(&self.resources, updated, observed)?;

        // 5-6. Symbol continuity against the previous set, then the
        //      Resource-owned replacement of Symbols and Occurrences in
        //      one write.
        let profile_id = symbol::ensure_profile(&transaction, &extraction.profile)?;
        let previous = symbol::list_for_resource(&transaction, updated.id)?;
        let symbols = extract::assign_ids(&previous, extraction, updated, profile_id);
        let occurrences =
            extract::resolve_occurrences(extraction, &symbols, updated, profile_id, building.id);
        symbol::replace_structure_in_publication(
            &transaction,
            &grant,
            &publication.revision,
            updated.id,
            &updated.resource_revision,
            &symbols,
            &occurrences,
        )?;

        // 7-9. The candidates this generation accounts for, the component
        //      it makes current, and the generation itself.
        watch::mark_applied(&transaction, input.journal_seq, Some(building.id))?;
        component::mark_current(&transaction, &publication.revision, building.id)?;
        let published = generation::finish_publish_stable(&transaction, &building)?;

        transaction.commit().map_err(ResourceError::from)?;
        Ok(RefreshOutcome::Published(RefreshPublication {
            resource_id: updated.id,
            path_rel: updated.path_rel.clone(),
            resource_revision: updated.resource_revision.clone(),
            workspace_revision: publication.revision.clone(),
            generation: published,
            symbols: symbols.len(),
            occurrences: occurrences.len(),
        }))
    }

    /// The verified-identical path: the file was touched, its content is
    /// what is already indexed. No revision, no generation -- just the
    /// bookkeeping the verification earned.
    fn finish_no_op(
        &self,
        input: &InputSnapshot,
        target: &Target,
        changes: &[ResourceChange],
    ) -> Result<RefreshOutcome, RefreshError> {
        let transaction = self.resources.transaction()?;
        if let Some(reason) = verify_clock_and_journal(&transaction, input)? {
            return Ok(RefreshOutcome::Obsolete(reason));
        }

        // Only `MetadataRefresh`/`Unchanged` reach here: size and mtime
        // are fast-path hints, so refreshing them advances no revision and
        // changes no fingerprint.
        identity::apply_in_transaction(&self.resources, changes)?;

        let stable = generation::current_stable(&transaction)?.ok_or_else(|| {
            RefreshError::InvariantViolated {
                detail: "a no-op refresh requires an existing stable generation".to_owned(),
            }
        })?;
        // Every pending candidate belonged to this target (eligibility
        // refused anything else), and its content is verified, so the
        // component may be called current again. No generation published
        // it, so none is attributed to the journal rows.
        watch::mark_applied(&transaction, input.journal_seq, None)?;
        component::mark_current(&transaction, &input.workspace_revision, stable.id)?;

        transaction.commit().map_err(ResourceError::from)?;
        Ok(RefreshOutcome::NoOp {
            resource_id: target.resource.id,
            path_rel: target.resource.path_rel.clone(),
        })
    }

    /// After a failed publication the index is whatever it was before,
    /// which is exactly what could not be confirmed -- so it stays
    /// DIRTY/QUEUED and reconcile is still required. An existing error
    /// code is kept: a later failure does not replace the earlier reason.
    fn mark_recovery_required(&self) -> Result<(), RefreshError> {
        let revision = generation::current_workspace_revision(self.connection())?
            .ok_or(GenerationError::ClockNotBootstrapped)?;
        let unexplained =
            component::read(self.connection())?.is_none_or(|state| state.last_error_code.is_none());
        component::mark_dirty(
            self.connection(),
            &revision,
            unexplained.then_some(component::RECONCILE_FAILED_CODE),
        )?;
        Ok(())
    }

    fn snapshot_input(&self) -> Result<InputSnapshot, RefreshError> {
        Ok(InputSnapshot {
            workspace_revision: generation::current_workspace_revision(self.connection())?
                .ok_or(GenerationError::ClockNotBootstrapped)?,
            change_seq: generation::change_seq(self.connection())?,
            journal_seq: watch::max_journal_seq(self.connection())?,
        })
    }

    fn connection(&self) -> &Connection {
        self.resources.connection()
    }
}

/// The single eligible target, or the reason there is not one.
type Selected = Result<Target, DeferReason>;

/// The one Resource this refresh is about, as persisted and as classified
/// on disk right now.
struct Target {
    resource: Resource,
    discovered: DiscoveredResource,
}

/// What the analysis was made against, so the publication can prove none
/// of it moved before committing.
struct InputSnapshot {
    workspace_revision: String,
    change_seq: i64,
    journal_seq: i64,
}

/// The generation being published and the revision advance it carries.
struct Publication {
    generation_id: i64,
    change_seq: i64,
    revision: String,
}

/// The first pending candidate this path cannot handle, if there is one.
///
/// A MOVE, a BULK_HINT, a CREATE, or a DELETE all mean the identity
/// question is open, and answering it is reconcile's job.
fn unsupported_candidate(pending: &[JournalEntry]) -> Option<DeferReason> {
    for entry in pending {
        match entry.event_kind {
            WatchEventKind::Modify => {}
            kind => return Some(DeferReason::UnsupportedEvent { kind }),
        }
        if entry.resource_id.is_none() {
            return Some(DeferReason::NotAnExistingResource {
                path: entry
                    .path_after
                    .clone()
                    .or_else(|| entry.path_before.clone()),
            });
        }
    }
    None
}

/// The clock, the journal, and the target file, all still as the analysis
/// assumed. `Ok(None)` means nothing moved.
fn verify_unchanged(
    transaction: &Connection,
    workspace_root: &Path,
    config: &WorkspaceConfig,
    input: &InputSnapshot,
    target: &Target,
    observed: &ObservedResource,
) -> Result<Option<ObsoleteReason>, RefreshError> {
    if let Some(reason) = verify_clock_and_journal(transaction, input)? {
        return Ok(Some(reason));
    }

    // One path re-classified and one file re-hashed -- not a Workspace.
    if discovery::is_ignored_path(workspace_root, &target.resource.path_rel, config) {
        return Ok(Some(ObsoleteReason::TargetChangedDuringRefresh {
            path_rel: target.resource.path_rel.clone(),
            detail: "the path became excluded during the refresh".to_owned(),
        }));
    }
    let Some(discovered) = discovery::describe_path(workspace_root, &target.resource.path_rel)?
    else {
        return Ok(Some(ObsoleteReason::TargetChangedDuringRefresh {
            path_rel: target.resource.path_rel.clone(),
            detail: "the file disappeared during the refresh".to_owned(),
        }));
    };
    let fresh = identity::observe(
        workspace_root,
        &discovered,
        Some(&target.resource),
        ObservationMode::Verified,
    )?;
    if let Some(detail) =
        scan::snapshot_drift(std::slice::from_ref(observed), std::slice::from_ref(&fresh))
    {
        return Ok(Some(ObsoleteReason::TargetChangedDuringRefresh {
            path_rel: target.resource.path_rel.clone(),
            detail,
        }));
    }
    Ok(None)
}

fn verify_clock_and_journal(
    transaction: &Connection,
    input: &InputSnapshot,
) -> Result<Option<ObsoleteReason>, RefreshError> {
    let current = generation::current_workspace_revision(transaction)?
        .ok_or(GenerationError::ClockNotBootstrapped)?;
    if current != input.workspace_revision {
        return Ok(Some(ObsoleteReason::ClockAdvanced {
            detail: format!(
                "workspace revision moved from {:?} to {current:?} during the refresh",
                input.workspace_revision
            ),
        }));
    }
    if generation::change_seq(transaction)? != input.change_seq {
        return Ok(Some(ObsoleteReason::ClockAdvanced {
            detail: "the input change sequence advanced during the refresh".to_owned(),
        }));
    }
    let journal_seq = watch::max_journal_seq(transaction)?;
    if journal_seq != input.journal_seq {
        return Ok(Some(ObsoleteReason::JournalAdvanced {
            from_seq: input.journal_seq,
            to_seq: journal_seq,
        }));
    }
    Ok(None)
}

/// The targeted equivalent of the baseline's invariant pass: the one row
/// this publication wrote describes the bytes it was published from.
fn verify_target_row(
    resources: &ResourceStore,
    updated: &Resource,
    observed: &ObservedResource,
) -> Result<(), RefreshError> {
    let stored =
        resources
            .get_by_id(updated.id)?
            .ok_or_else(|| RefreshError::InvariantViolated {
                detail: format!("resource {} vanished during its own update", updated.id),
            })?;
    if stored.state != crate::resource::ResourceState::Active
        || stored.path_key != observed.discovered.path_key
        || stored.content_hash != observed.content_hash
        || stored.resource_revision != updated.resource_revision
    {
        return Err(RefreshError::InvariantViolated {
            detail: format!(
                "resource {:?} does not describe the published bytes after its update",
                stored.path_rel
            ),
        });
    }
    Ok(())
}

fn obsolete_reason_text(reason: &ObsoleteReason) -> String {
    match reason {
        ObsoleteReason::TargetChangedDuringRefresh { path_rel, detail } => {
            format!("target {path_rel} changed during the refresh: {detail}")
        }
        ObsoleteReason::JournalAdvanced { from_seq, to_seq } => {
            format!("the watcher journalled candidates {from_seq}..{to_seq} during the refresh")
        }
        ObsoleteReason::ClockAdvanced { detail } => detail.clone(),
    }
}

/// The next Workspace revision: the input-change sequence itself, exactly
/// as reconcile computes it -- there is one revision scheme, not two.
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
        env,
        path::PathBuf,
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use brainprint_core::SymbolId;

    use super::*;
    use crate::{
        component::{FreshnessState, ProcessingState},
        generation::GenerationState,
        inspect::SourceReader,
        parser::dialect_for_resource,
        resource::ResourceState,
        scan::BaselineScan,
        symbol::{Occurrence, Symbol, SymbolStore},
        watch::{JournalState, RawWatchEvent, WatchIngest},
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    const BASELINE_REVISION: &str = "workspace-rev-1";

    const APP_TS: &str = "\
export class App {
  run(): number {
    return helper()
  }
}

function helper(): number {
  return 41
}
";

    /// The same file with a changed body: same declarations, shifted
    /// spans. Symbol identity must survive this.
    const APP_TS_BODY_EDIT: &str = "\
export class App {
  // a new comment line
  run(): number {
    return helper()
  }
}

function helper(): number {
  return 42
}
";

    /// One declaration removed, one added.
    const APP_TS_STRUCTURAL_EDIT: &str = "\
export class App {
  run(): number {
    return 1
  }
}

function added(): number {
  return 2
}
";

    const LIB_PY: &str = "\
def run():
    return 4
";

    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-refresh-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            std::fs::create_dir_all(root.join("src")).expect("src");
            std::fs::create_dir_all(root.join("docs")).expect("docs");
            let fixture = Self { base, root };
            fixture.write("src/app.ts", APP_TS);
            fixture.write("lib.py", LIB_PY);
            fixture.write("docs/readme.md", "# notes\n");
            fixture
        }

        fn db_path(&self) -> PathBuf {
            self.base.join("data").join("index.db")
        }

        fn write(&self, rel: &str, contents: &str) {
            std::fs::write(self.root.join(rel), contents).expect("fixture file");
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }

        /// Publish the Resource baseline, then the structural index of
        /// every supported file against the stable generation.
        fn index(&self) {
            let engine = BaselineScan::open(&self.db_path()).expect("index.db");
            engine
                .run_initial_scan(&self.root, &WorkspaceConfig::default(), BASELINE_REVISION)
                .expect("baseline scan");
            drop(engine);

            let store = SymbolStore::open(&self.db_path()).expect("index.db");
            let generation = generation::current_stable(store.connection())
                .expect("stable")
                .expect("the baseline published one")
                .id;
            for rel in ["src/app.ts", "lib.py"] {
                let resource = self.resource(rel);
                let source = std::fs::read(self.path(rel)).expect("current source");
                let dialect = dialect_for_resource(&resource).expect("a supported dialect");
                let tree = ParserRegistry::new()
                    .parse(dialect, &source, SourceBasis::of(&resource))
                    .expect("parse");
                let extraction = extract::extract(&tree, &source);
                let profile_id = store.ensure_profile(&extraction.profile).expect("profile");
                let symbols = extract::assign_ids(&[], &extraction, &resource, profile_id);
                let occurrences = extract::resolve_occurrences(
                    &extraction,
                    &symbols,
                    &resource,
                    profile_id,
                    generation,
                );
                store
                    .replace_structure(
                        resource.id,
                        &resource.resource_revision,
                        generation,
                        &symbols,
                        &occurrences,
                    )
                    .expect("replace");
            }
        }

        fn resource(&self, rel: &str) -> Resource {
            ResourceStore::open(&self.db_path())
                .expect("index.db")
                .get_active_by_path_key(rel)
                .expect("lookup")
                .expect("the fixture file is a Resource")
        }

        fn ingest(&self, events: &[RawWatchEvent]) {
            let ingest = WatchIngest::open(&self.db_path()).expect("index.db");
            ingest
                .ingest_all(&self.root, &WorkspaceConfig::default(), events)
                .expect("ingestion");
        }

        fn modify(&self, rel: &str, contents: &str) {
            self.write(rel, contents);
            self.ingest(&[RawWatchEvent::Modified {
                path: self.path(rel),
            }]);
        }

        fn engine(&self) -> TargetedRefresh {
            TargetedRefresh::open(&self.db_path()).expect("index.db")
        }

        fn run(&self) -> RefreshOutcome {
            self.engine()
                .run(&self.root, &WorkspaceConfig::default())
                .expect("refresh should not fail")
        }

        fn symbols(&self, rel: &str) -> Vec<Symbol> {
            SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_for_resource(self.resource(rel).id)
                .expect("symbols")
        }

        fn occurrences(&self, rel: &str) -> Vec<Occurrence> {
            SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_occurrences_for_resource(self.resource(rel).id)
                .expect("occurrences")
        }

        fn by_qualified_name(&self, rel: &str) -> BTreeMap<String, SymbolId> {
            self.symbols(rel)
                .into_iter()
                .map(|symbol| (symbol.qualified_name, symbol.id))
                .collect()
        }

        fn state(&self) -> IndexState {
            let store = ResourceStore::open(&self.db_path()).expect("index.db");
            let connection = store.connection();
            let component = component::read(connection)
                .expect("component")
                .expect("published");
            IndexState {
                workspace_revision: generation::current_workspace_revision(connection)
                    .expect("clock")
                    .expect("bootstrapped"),
                change_seq: generation::change_seq(connection).expect("clock"),
                stable_generation: generation::current_stable(connection)
                    .expect("stable")
                    .map(|generation| generation.id),
                freshness: component.freshness_state,
                processing: component.processing_state,
                journal: watch::journal(connection)
                    .expect("journal")
                    .into_iter()
                    .map(|entry| {
                        (
                            entry.seq,
                            entry.processing_state,
                            entry.applied_generation_id,
                        )
                    })
                    .collect(),
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct IndexState {
        workspace_revision: String,
        change_seq: i64,
        stable_generation: Option<i64>,
        freshness: FreshnessState,
        processing: ProcessingState,
        journal: Vec<(i64, JournalState, Option<i64>)>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn generation_states(fixture: &Fixture) -> Vec<(i64, GenerationState)> {
        let store = ResourceStore::open(&fixture.db_path()).expect("index.db");
        let mut statement = store
            .connection()
            .prepare("SELECT id, state FROM generation ORDER BY id")
            .expect("prepare");
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query");
        rows.map(|row| {
            let (id, state) = row.expect("row");
            (
                id,
                match state.as_str() {
                    "BUILDING" => GenerationState::Building,
                    "STABLE" => GenerationState::Stable,
                    "ABORTED" => GenerationState::Aborted,
                    other => panic!("unexpected generation state {other}"),
                },
            )
        })
        .collect()
    }

    #[test]
    fn one_saved_file_is_republished_without_walking_the_workspace() {
        let fixture = Fixture::create("single-save");
        fixture.index();
        let before = fixture.resource("src/app.ts");
        let before_state = fixture.state();
        fixture.modify("src/app.ts", APP_TS_BODY_EDIT);

        // The proof that no other Resource is read, hashed, or parsed:
        // every other file is gone. A full discovery pass would see two
        // deletions; a targeted refresh never looks.
        std::fs::remove_file(fixture.path("lib.py")).expect("remove");
        std::fs::remove_file(fixture.path("docs/readme.md")).expect("remove");

        let outcome = fixture.run();

        let published = outcome.published().expect("a targeted publication");
        assert_eq!(published.resource_id, before.id, "the stable id is kept");
        assert_eq!(published.path_rel, "src/app.ts");

        let after = fixture.resource("src/app.ts");
        assert_eq!(after.id, before.id);
        assert_eq!(
            after.resource_revision,
            identity::next_revision(&before.resource_revision).expect("revision")
        );
        assert_eq!(published.resource_revision, after.resource_revision);

        // The other Resources were not touched in any way.
        assert_eq!(fixture.resource("lib.py").state, ResourceState::Active);
        assert_eq!(fixture.symbols("lib.py").len(), 1);

        let state = fixture.state();
        assert_eq!(
            state.change_seq,
            before_state.change_seq + 1,
            "exactly one input change"
        );
        assert_eq!(state.workspace_revision, state.change_seq.to_string());
        assert_ne!(state.workspace_revision, before_state.workspace_revision);
        assert_eq!(published.workspace_revision, state.workspace_revision);
        assert_eq!(state.freshness, FreshnessState::Current);
        assert_eq!(state.processing, ProcessingState::Ready);
        assert_eq!(state.stable_generation, Some(published.generation.id));
        assert_eq!(published.generation.state, GenerationState::Stable);
        assert_eq!(
            state.journal,
            vec![(1, JournalState::Applied, Some(published.generation.id))],
            "the candidate is resolved, attributed to the generation that published it"
        );
        assert!(
            generation_states(&fixture)
                .iter()
                .all(|(_, state)| *state != GenerationState::Building),
            "no BUILDING generation is left behind, let alone exposed as current"
        );
    }

    #[test]
    fn a_body_edit_keeps_every_symbol_id_and_moves_their_spans() {
        let fixture = Fixture::create("body-edit");
        fixture.index();
        let before = fixture.by_qualified_name("src/app.ts");
        let before_run = fixture
            .symbols("src/app.ts")
            .into_iter()
            .find(|symbol| symbol.qualified_name == "App.run")
            .expect("App.run");
        fixture.modify("src/app.ts", APP_TS_BODY_EDIT);

        let published = fixture.run();
        assert!(published.published().is_some());

        let after = fixture.by_qualified_name("src/app.ts");
        assert_eq!(after, before, "same declarations, same SymbolIds");

        let after_run = fixture
            .symbols("src/app.ts")
            .into_iter()
            .find(|symbol| symbol.qualified_name == "App.run")
            .expect("App.run");
        assert!(
            after_run.span.start_byte > before_run.span.start_byte,
            "the declaration moved down the file"
        );
        assert_eq!(after_run.span.start.line, 2);
        assert_eq!(
            after_run.resource_revision,
            fixture.resource("src/app.ts").resource_revision
        );

        // And a read of that span now returns the edited source, with no
        // second lookup and no refresh in between.
        let reader = SourceReader::open(&fixture.db_path(), &fixture.root).expect("reader");
        let inspected = reader.inspect_symbol(after_run.id).expect("inspect");
        assert_eq!(
            inspected.source,
            "run(): number {\n    return helper()\n  }"
        );
        assert!(inspected.verification.currentness.is_current());
    }

    #[test]
    fn a_structural_edit_replaces_the_symbol_set_and_its_occurrences() {
        let fixture = Fixture::create("structural-edit");
        fixture.index();
        let before = fixture.by_qualified_name("src/app.ts");
        assert!(before.contains_key("helper"));
        let before_generation = fixture.state().stable_generation.expect("stable");
        assert!(
            fixture
                .occurrences("src/app.ts")
                .iter()
                .all(|occurrence| occurrence.generation_id == before_generation)
        );
        fixture.modify("src/app.ts", APP_TS_STRUCTURAL_EDIT);

        let outcome = fixture.run();
        let published = outcome.published().expect("published");

        let after = fixture.by_qualified_name("src/app.ts");
        assert!(
            !after.contains_key("helper"),
            "the removed declaration is gone"
        );
        assert!(after.contains_key("added"), "the new declaration is there");
        assert_eq!(
            after.get("App.run"),
            before.get("App.run"),
            "the surviving declaration keeps its identity"
        );
        let added = fixture
            .symbols("src/app.ts")
            .into_iter()
            .find(|symbol| symbol.qualified_name == "added")
            .expect("added");
        assert_eq!(
            &APP_TS_STRUCTURAL_EDIT[added.span.start_byte..added.span.end_byte],
            "function added(): number {\n  return 2\n}"
        );

        let occurrences = fixture.occurrences("src/app.ts");
        assert!(!occurrences.is_empty());
        assert!(
            occurrences
                .iter()
                .all(|occurrence| occurrence.generation_id == published.generation.id),
            "the evidence names the generation that published it"
        );
        assert_ne!(published.generation.id, before_generation);
        assert!(
            occurrences
                .iter()
                .all(|occurrence| occurrence.resource_revision
                    == fixture.resource("src/app.ts").resource_revision)
        );
        assert_eq!(published.occurrences, occurrences.len());
        assert_eq!(published.symbols, fixture.symbols("src/app.ts").len());
    }

    #[test]
    fn a_touch_that_changed_nothing_publishes_nothing() {
        let fixture = Fixture::create("no-op");
        fixture.index();
        let before = fixture.state();
        let before_symbols = fixture.by_qualified_name("src/app.ts");
        // Rewritten with identical content: a real MODIFY candidate whose
        // verified content matches what is indexed.
        fixture.modify("src/app.ts", APP_TS);

        let outcome = fixture.run();

        assert!(
            matches!(outcome, RefreshOutcome::NoOp { ref path_rel, .. } if path_rel == "src/app.ts"),
            "unexpected outcome: {outcome:?}"
        );
        let after = fixture.state();
        assert_eq!(after.workspace_revision, before.workspace_revision);
        assert_eq!(after.change_seq, before.change_seq);
        assert_eq!(after.stable_generation, before.stable_generation);
        assert_eq!(
            fixture.resource("src/app.ts").resource_revision,
            "1",
            "no resource revision moved either"
        );
        assert_eq!(fixture.by_qualified_name("src/app.ts"), before_symbols);
        assert_eq!(generation_states(&fixture).len(), 1, "no new generation");
        // The candidate is resolved and the component is current again.
        assert_eq!(after.journal, vec![(1, JournalState::Applied, None)]);
        assert_eq!(after.freshness, FreshnessState::Current);
    }

    #[test]
    fn a_second_save_during_the_refresh_discards_the_analysis() {
        let fixture = Fixture::create("second-save");
        fixture.index();
        fixture.modify("src/app.ts", APP_TS_BODY_EDIT);
        let before = fixture.state();
        let before_symbols = fixture.by_qualified_name("src/app.ts");

        // The interleaving, step by step: everything the refresh does
        // before its transaction, then a second save, then the
        // publication attempt that is supposed to notice.
        let engine = fixture.engine();
        let config = WorkspaceConfig::default();
        let input = engine.snapshot_input().expect("snapshot");
        let target = engine
            .select_target(&fixture.root, &config)
            .expect("select")
            .expect("eligible");
        let observed = identity::observe(
            &fixture.root,
            &target.discovered,
            Some(&target.resource),
            ObservationMode::Verified,
        )
        .expect("observe");
        let bytes = std::fs::read(fixture.path("src/app.ts")).expect("read");

        fixture.modify("src/app.ts", APP_TS_STRUCTURAL_EDIT);

        let changes = identity::plan_changes(
            std::slice::from_ref(&target.resource),
            std::slice::from_ref(&observed),
            &[],
        )
        .expect("plan");
        let [ResourceChange::Update(updated)] = changes.as_slice() else {
            panic!("expected a single update, got {changes:?}");
        };
        let outcome = engine
            .refresh_changed(
                &fixture.root,
                &config,
                &input,
                &target,
                updated,
                &observed,
                &bytes,
            )
            .expect("an obsolete refresh is a normal outcome, not an error");

        assert!(
            matches!(outcome, RefreshOutcome::Obsolete(_)),
            "unexpected outcome: {outcome:?}"
        );
        let after = fixture.state();
        assert_eq!(after.workspace_revision, before.workspace_revision);
        assert_eq!(after.change_seq, before.change_seq);
        assert_eq!(after.stable_generation, before.stable_generation);
        assert_eq!(
            fixture.by_qualified_name("src/app.ts"),
            before_symbols,
            "stale analysis is not published"
        );
        assert!(
            after
                .journal
                .iter()
                .all(|(_, state, _)| *state != JournalState::Applied),
            "nothing was accounted for, so the candidates stay outstanding \
             (a coalesced row is still unapplied evidence)"
        );
        assert_eq!(after.freshness, FreshnessState::Dirty);
        assert_eq!(
            generation_states(&fixture)
                .into_iter()
                .filter(|(_, state)| *state == GenerationState::Aborted)
                .count(),
            1,
            "the started generation is explicitly aborted, never left BUILDING"
        );
    }

    #[test]
    fn an_unrelated_pending_resource_defers_instead_of_claiming_the_workspace_is_current() {
        let fixture = Fixture::create("two-resources");
        fixture.index();
        fixture.modify("src/app.ts", APP_TS_BODY_EDIT);
        fixture.modify("lib.py", "def run():\n    return 5\n");
        let before = fixture.state();

        let outcome = fixture.run();

        assert!(
            matches!(
                outcome,
                RefreshOutcome::Deferred(DeferReason::MultiplePendingResources { resources: 2 })
            ),
            "unexpected outcome: {outcome:?}"
        );
        let after = fixture.state();
        assert_eq!(after, before, "nothing at all was written");
        assert_eq!(after.freshness, FreshnessState::Dirty);
    }

    #[test]
    fn a_continuity_loss_or_a_bulk_hint_is_reconciles_problem() {
        let lost = Fixture::create("continuity-lost");
        lost.index();
        lost.write("src/app.ts", APP_TS_BODY_EDIT);
        lost.ingest(&[RawWatchEvent::ContinuityLost {
            detail: "queue overflow".to_owned(),
        }]);
        assert!(
            matches!(
                lost.run(),
                RefreshOutcome::Deferred(DeferReason::WatcherContinuityLost)
            ),
            "a lost event stream means the pending set is not the change set"
        );
        assert_eq!(lost.state().freshness, FreshnessState::Dirty);

        // Even once the backend reports itself healthy again, the
        // BULK_HINT candidate it left behind is still a whole-Workspace
        // question that only reconcile can answer.
        let ingest = WatchIngest::open(&lost.db_path()).expect("index.db");
        ingest
            .mark_watcher_continuous()
            .expect("the backend recovers");
        drop(ingest);
        assert!(
            matches!(
                lost.run(),
                RefreshOutcome::Deferred(DeferReason::UnsupportedEvent {
                    kind: WatchEventKind::BulkHint
                })
            ),
            "a bulk hint is not a single-file modify"
        );
    }

    #[test]
    fn a_move_or_a_delete_then_create_defers_rather_than_guessing_identity() {
        let moved = Fixture::create("moved");
        moved.index();
        std::fs::rename(moved.path("src/app.ts"), moved.path("src/renamed.ts")).expect("rename");
        moved.ingest(&[RawWatchEvent::RenamedPair {
            from: moved.path("src/app.ts"),
            to: moved.path("src/renamed.ts"),
        }]);
        assert!(
            matches!(
                moved.run(),
                RefreshOutcome::Deferred(DeferReason::UnsupportedEvent {
                    kind: WatchEventKind::Move
                })
            ),
            "a rename is an identity decision, and reconcile owns it"
        );

        let recreated = Fixture::create("delete-create");
        recreated.index();
        std::fs::remove_file(recreated.path("src/app.ts")).expect("remove");
        recreated.ingest(&[RawWatchEvent::Removed {
            path: recreated.path("src/app.ts"),
        }]);
        recreated.write("src/app.ts", APP_TS_STRUCTURAL_EDIT);
        recreated.ingest(&[RawWatchEvent::Created {
            path: recreated.path("src/app.ts"),
        }]);
        assert!(
            matches!(
                recreated.run(),
                RefreshOutcome::Deferred(DeferReason::UnsupportedEvent { .. })
            ),
            "a delete followed by a create is exactly the identity guess to avoid"
        );
        assert_eq!(
            recreated.symbols("src/app.ts").len(),
            3,
            "and the previously published structure is left alone"
        );
    }

    #[test]
    fn an_incomplete_parse_keeps_the_previously_accepted_structure() {
        let fixture = Fixture::create("partial-parse");
        fixture.index();
        let before_symbols = fixture.by_qualified_name("src/app.ts");
        let before_occurrences = fixture.occurrences("src/app.ts").len();
        let before = fixture.state();
        // A half-typed edit: a real, current file that the grammar cannot
        // accept as a whole.
        fixture.modify("src/app.ts", "export class App {\n  run(): number {\n");

        let outcome = fixture.run();

        assert!(
            matches!(
                outcome,
                RefreshOutcome::Deferred(DeferReason::ExtractionNotAccepted {
                    parse_status: ParseStatus::Partial,
                    ..
                })
            ),
            "unexpected outcome: {outcome:?}"
        );
        assert_eq!(
            fixture.by_qualified_name("src/app.ts"),
            before_symbols,
            "a broken edit never blanks a file's structure"
        );
        assert_eq!(fixture.occurrences("src/app.ts").len(), before_occurrences);
        let after = fixture.state();
        assert_eq!(after.change_seq, before.change_seq);
        assert_eq!(after.stable_generation, before.stable_generation);
        assert_eq!(after.freshness, FreshnessState::Dirty);
        assert!(
            after
                .journal
                .iter()
                .all(|(_, state, _)| *state == JournalState::Pending)
        );
    }

    #[test]
    fn a_failed_publication_rolls_back_everything_and_aborts_its_generation() {
        let fixture = Fixture::create("rollback");
        fixture.index();
        let before_symbols = fixture.by_qualified_name("src/app.ts");
        let before_occurrences = fixture.occurrences("src/app.ts").len();
        let before = fixture.state();
        fixture.modify("src/app.ts", APP_TS_STRUCTURAL_EDIT);

        // Injected failure at the last write of the publication, after the
        // Resource update and the Symbol replacement have already run.
        let engine = fixture.engine();
        engine
            .connection()
            .execute_batch(
                "CREATE TRIGGER fail_occurrence BEFORE INSERT ON occurrence \
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END",
            )
            .expect("trigger");

        let error = engine
            .run(&fixture.root, &WorkspaceConfig::default())
            .expect_err("the publication must fail");
        engine
            .connection()
            .execute_batch("DROP TRIGGER fail_occurrence")
            .expect("drop trigger");
        drop(engine);
        assert!(
            matches!(error, RefreshError::Symbol(_)),
            "unexpected: {error}"
        );

        let after = fixture.state();
        assert_eq!(after.workspace_revision, before.workspace_revision);
        assert_eq!(after.change_seq, before.change_seq);
        assert_eq!(after.stable_generation, before.stable_generation);
        assert_eq!(fixture.resource("src/app.ts").resource_revision, "1");
        assert_eq!(fixture.by_qualified_name("src/app.ts"), before_symbols);
        assert_eq!(fixture.occurrences("src/app.ts").len(), before_occurrences);
        assert!(
            after
                .journal
                .iter()
                .all(|(_, state, _)| *state == JournalState::Pending)
        );
        assert_eq!(after.freshness, FreshnessState::Dirty);
        assert_eq!(
            generation_states(&fixture)
                .into_iter()
                .filter(|(_, state)| *state == GenerationState::Aborted)
                .count(),
            1
        );
    }

    #[test]
    fn a_targeted_publication_stores_no_source_text() {
        let fixture = Fixture::create("no-source-mirror");
        fixture.index();
        fixture.modify("src/app.ts", APP_TS_STRUCTURAL_EDIT);
        assert!(fixture.run().published().is_some());

        let database = std::fs::read(fixture.db_path()).expect("index.db");
        for body in ["return 2", "function added(): number {\n  return 2\n}"] {
            assert!(
                !database
                    .windows(body.len())
                    .any(|window| window == body.as_bytes()),
                "index.db must not mirror source text ({body:?})"
            );
        }
    }
}
