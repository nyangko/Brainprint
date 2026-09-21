//! Resource fingerprint/revision/stable identity and the
//! create/modify/delete/move application rules (#16 task 3).
//!
//! Scope: turning a task 2 discovery snapshot plus the persisted task 1
//! Resource inventory into an explicit, reviewable list of identity
//! decisions ([`plan_changes`], pure) and writing that list through
//! [`crate::resource::ResourceStore`] ([`apply`]). It does not publish a
//! Workspace generation (#16 task 4), ingest watcher events or write
//! `change_journal` (#16 task 5), orchestrate reconcile correctness (#16
//! task 6), or parse anything (#16 task 7+).
//!
//! ## Fingerprint
//!
//! [`FINGERPRINT_ALGORITHM`]/[`CONTENT_HASH_ALGORITHM`] are tagged into the
//! stored strings precisely so the algorithm stays a swappable
//! implementation detail rather than architecture (#16 "exact hash policy는
//! benchmark-determined로 둔다"). What *is* locked:
//! - the fingerprint covers structural inputs only -- path, kind, role,
//!   language, size, content hash, generated kind, container -- and
//!   deliberately **excludes `mtime`**: mtime alone is never evidence of a
//!   semantic change.
//! - encoding is length-prefixed and byte-oriented, so it is deterministic
//!   and identical on every platform.
//! - the file's own bytes are hashed, never stored (#16 "source 원문을 DB에
//!   저장하지 않는다").
//! - a Directory Resource has no content hash, so its fingerprint depends on
//!   its own structural inputs only -- child edits never ripple into a
//!   parent directory's revision.
//!
//! mtime/size are recorded as a *cheap metadata fast path*, but that path
//! is never the only one available: [`observe`] takes an explicit
//! [`ObservationMode`]. [`ObservationMode::Fast`] may reuse a recorded
//! content hash when size and mtime both still match (the watcher's latency
//! path); [`ObservationMode::Verified`] always re-hashes a file's current
//! bytes (the initial scan / reconcile correctness path), so a write that
//! happens to preserve size and mtime is still caught. Metadata alone never
//! settles currentness.
//!
//! ## Identity
//!
//! `path` is not identity. The rules, in the order [`plan_changes`] applies
//! them:
//! - **create**: an observed path with no ACTIVE Resource gets a brand-new
//!   [`ResourceId`] at revision [`INITIAL_REVISION`].
//! - **modify**: an observed path with an ACTIVE Resource keeps that id; the
//!   revision advances only when the structural fingerprint actually
//!   differs.
//! - **delete**: an ACTIVE Resource with no observation becomes a tombstone
//!   -- same id, one revision bump, never handed to another file. Repeating
//!   it is a no-op ([`crate::resource::ResourceStore::mark_deleted`]).
//! - **move**: only [`MoveEvidence`] the caller vouches for keeps an id
//!   across a path change, and only when it is unambiguous. Identical
//!   content is *not* evidence. Without it the old path is deleted and the
//!   new path is created -- a false split is recoverable, a false merge is
//!   not.
//! - **delete → same-path recreate**: the tombstone is never resurrected;
//!   the recreated path is a new id. Editor atomic-save continuity is
//!   expressible later by handing [`MoveEvidence`] in from the watcher or
//!   reconcile stage (#16 task 5-6) -- no new API is needed for it.

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use brainprint_core::ResourceId;
use sha2::{Digest, Sha256};

use crate::{
    discovery::DiscoveredResource,
    resource::{
        Resource, ResourceError, ResourceKind, ResourceLanguage, ResourceRole, ResourceState,
        ResourceStore,
    },
};

/// Tag prefixed to every stored `resource.fingerprint`. Bumping it (or
/// swapping the hash behind it) is a benchmark decision, not an
/// architectural one -- stored values stay self-describing either way.
pub const FINGERPRINT_ALGORITHM: &str = "sha256-fp1";

/// Tag prefixed to every stored `resource.content_hash`.
pub const CONTENT_HASH_ALGORITHM: &str = "sha256";

/// `resource_revision` assigned to a newly created Resource.
pub const INITIAL_REVISION: &str = "1";

/// Filesystem facts observed for one discovered entry, ready to be turned
/// into an identity decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedResource {
    pub discovered: DiscoveredResource,
    pub size_bytes: i64,
    /// Cheap fast-path hint only -- never fingerprint input.
    pub mtime_ns: i64,
    /// `None` for a Directory Resource, which has no content of its own.
    pub content_hash: Option<String>,
}

/// Caller-supplied, high-confidence evidence that one path *is* another
/// path's Resource moved (an explicit rename event, a VCS rename record,
/// ...). Producing it is the watcher/reconcile stage's job (#16 task 5-6);
/// this module only consumes it, and only when it is unambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveEvidence {
    pub from_path_key: String,
    pub to_path_key: String,
}

/// One identity decision for one Resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceChange {
    /// No ACTIVE Resource held this path: a new stable id.
    Create(Resource),
    /// Structural inputs changed (content, role, language, kind, container,
    /// or -- for an evidenced move -- the path): same id, next revision.
    Update(Resource),
    /// Structural inputs are identical but mtime/size drifted: revision and
    /// fingerprint stay put, only the fast-path metadata is refreshed.
    MetadataRefresh {
        id: ResourceId,
        size_bytes: i64,
        mtime_ns: i64,
    },
    /// Complete no-op.
    Unchanged { id: ResourceId },
    /// ACTIVE → DELETED tombstone: same id, next revision, path freed.
    Delete {
        id: ResourceId,
        resource_revision: String,
    },
}

/// Failure observing the filesystem, decoding a revision, or persisting a
/// planned change.
#[derive(Debug)]
pub enum IdentityError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A stored `resource_revision` was not the decimal counter this module
    /// writes. Never silently restarted from zero -- that would make an
    /// advancing revision look unchanged.
    UnreadableRevision {
        raw: String,
    },
    Store(ResourceError),
}

impl fmt::Display for IdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(
                    formatter,
                    "identity I/O failed at {}: {source}",
                    path.display()
                )
            }
            Self::UnreadableRevision { raw } => {
                write!(
                    formatter,
                    "resource_revision {raw:?} is not a decimal counter"
                )
            }
            Self::Store(source) => write!(formatter, "resource store failed: {source}"),
        }
    }
}

impl Error for IdentityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::UnreadableRevision { .. } => None,
            Self::Store(source) => Some(source),
        }
    }
}

impl From<ResourceError> for IdentityError {
    fn from(source: ResourceError) -> Self {
        Self::Store(source)
    }
}

/// How hard [`observe`] works to establish a file's current content (#16
/// "watcher는 latency fast path, reconcile은 correctness path" /
/// "mtime만으로 currentness를 확정하지 않는다").
///
/// This selects the *observation* effort only. The fingerprint, revision,
/// and identity rules downstream are identical either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationMode {
    /// Latency path: a file whose size *and* mtime still match the
    /// persisted ACTIVE row may reuse that row's recorded content hash.
    /// Cheap, and wrong exactly when a write preserves both size and
    /// mtime -- which is why it must never be the only path available.
    /// For targeted watcher refreshes (#16 task 5).
    Fast,
    /// Correctness path: every FILE's current bytes are hashed, whatever
    /// the metadata says. For the initial scan and reconcile (#16 task
    /// 4/6). A DIRECTORY is unaffected -- it has no content of its own and
    /// still never hashes its children.
    Verified,
}

/// Read the filesystem facts for one discovered entry under
/// `workspace_root`.
///
/// `previous` is only consulted under [`ObservationMode::Fast`], where a
/// persisted ACTIVE row agreeing on both size and mtime lets its recorded
/// content hash be reused. Anything less certain -- and everything under
/// [`ObservationMode::Verified`] -- re-reads and re-hashes the actual
/// bytes.
pub fn observe(
    workspace_root: &Path,
    discovered: &DiscoveredResource,
    previous: Option<&Resource>,
    mode: ObservationMode,
) -> Result<ObservedResource, IdentityError> {
    let path = workspace_root.join(&discovered.path_rel);
    let metadata = fs::metadata(&path).map_err(|source| io_error(&path, source))?;
    // A directory's own size/mtime move whenever a *child* is added or
    // removed, so recording them would make a parent directory's state
    // follow its children (#16 task 3: a directory Resource is defined by
    // its own structural inputs). It has no content to fast-path either,
    // so both are pinned to 0.
    let directory = discovered.kind == ResourceKind::Directory;
    let size_bytes = if directory {
        0
    } else {
        i64::try_from(metadata.len()).unwrap_or(i64::MAX)
    };
    let mtime_ns = if directory { 0 } else { mtime_nanos(&metadata) };

    let content_hash = match discovered.kind {
        ResourceKind::Directory => None,
        ResourceKind::File => {
            let reusable = previous.filter(|previous| {
                mode == ObservationMode::Fast
                    && previous.state == ResourceState::Active
                    && previous.size_bytes == size_bytes
                    && previous.mtime_ns == mtime_ns
            });
            match reusable.and_then(|previous| previous.content_hash.clone()) {
                Some(hash) => Some(hash),
                None => Some(hash_file(&path)?),
            }
        }
    };

    Ok(ObservedResource {
        discovered: discovered.clone(),
        size_bytes,
        mtime_ns,
        content_hash,
    })
}

/// Decide what happens to every Resource, given the current ACTIVE
/// inventory, a full observation of the Workspace, and whatever move
/// evidence the caller can vouch for. Pure: no I/O, no DB, no clock.
///
/// Results are ordered by `path_key`, then deletions, for determinism.
pub fn plan_changes(
    active: &[Resource],
    observed: &[ObservedResource],
    move_evidence: &[MoveEvidence],
) -> Result<Vec<ResourceChange>, IdentityError> {
    let by_path: HashMap<&str, &Resource> = active
        .iter()
        .filter(|resource| resource.state == ResourceState::Active)
        .map(|resource| (resource.path_key.as_str(), resource))
        .collect();
    let observed_paths: HashSet<&str> = observed
        .iter()
        .map(|entry| entry.discovered.path_key.as_str())
        .collect();
    let accepted_moves = accept_moves(&by_path, &observed_paths, move_evidence);
    let move_sources: HashSet<&str> = accepted_moves.values().copied().collect();

    let mut changes = Vec::with_capacity(observed.len());
    for entry in observed {
        let path_key = entry.discovered.path_key.as_str();
        let previous = accepted_moves
            .get(path_key)
            .and_then(|from| by_path.get(from).copied())
            .or_else(|| by_path.get(path_key).copied());
        changes.push(match previous {
            Some(previous) => modify(previous, entry)?,
            None => ResourceChange::Create(build_resource(
                ResourceId::generate(),
                INITIAL_REVISION.to_owned(),
                entry,
                None,
                None,
            )),
        });
    }

    let mut deletions: Vec<&Resource> = by_path
        .values()
        .copied()
        .filter(|resource| {
            !observed_paths.contains(resource.path_key.as_str())
                && !move_sources.contains(resource.path_key.as_str())
        })
        .collect();
    deletions.sort_by(|left, right| left.path_key.cmp(&right.path_key));
    for resource in deletions {
        changes.push(ResourceChange::Delete {
            id: resource.id,
            resource_revision: next_revision(&resource.resource_revision)?,
        });
    }

    Ok(changes)
}

/// Persist a plan through `store`, in one transaction and in an order that
/// frees a deleted path before anything else claims it: deletes, then
/// updates (which include evidenced moves), then creates.
pub fn apply(store: &ResourceStore, changes: &[ResourceChange]) -> Result<(), IdentityError> {
    let transaction = store.transaction()?;
    apply_in_transaction(store, changes)?;
    transaction.commit().map_err(ResourceError::from)?;
    Ok(())
}

/// [`apply`]'s writes without the transaction, for a caller that already
/// owns one on the same connection and must commit other `index.db` rows
/// alongside them (#16 task 4's atomic baseline publication).
pub fn apply_in_transaction(
    store: &ResourceStore,
    changes: &[ResourceChange],
) -> Result<(), IdentityError> {
    for change in changes {
        if let ResourceChange::Delete {
            id,
            resource_revision,
        } = change
        {
            store.mark_deleted(*id, resource_revision)?;
        }
    }
    for change in changes {
        match change {
            ResourceChange::Update(resource) => {
                store.update_resource(resource)?;
            }
            ResourceChange::MetadataRefresh {
                id,
                size_bytes,
                mtime_ns,
            } => {
                store.refresh_metadata(*id, *size_bytes, *mtime_ns)?;
            }
            _ => {}
        }
    }
    for change in changes {
        if let ResourceChange::Create(resource) = change {
            store.insert_resource(resource)?;
        }
    }

    Ok(())
}

/// The next value of a `resource_revision` counter.
pub fn next_revision(current: &str) -> Result<String, IdentityError> {
    let parsed: u64 = current
        .parse()
        .map_err(|_| IdentityError::UnreadableRevision {
            raw: current.to_owned(),
        })?;
    Ok((parsed + 1).to_string())
}

/// Candidate structural fingerprint for a freshly observed entry, using
/// `previous`'s non-discovery attributes (generated kind, container) where
/// one exists -- so the value is comparable with that row's stored
/// `fingerprint`. A *candidate* only: computing it decides nothing (#16
/// task 5 journals it as evidence, not truth).
#[must_use]
pub fn observed_fingerprint(observed: &ObservedResource, previous: Option<&Resource>) -> String {
    build_resource(
        previous.map_or_else(ResourceId::generate, |previous| previous.id),
        INITIAL_REVISION.to_owned(),
        observed,
        previous.and_then(|previous| previous.generated_kind.clone()),
        previous.and_then(|previous| previous.container_resource_id),
    )
    .fingerprint
}

/// Structural fingerprint of a Resource as stored. Excludes `mtime_ns`.
#[must_use]
pub fn fingerprint_of(resource: &Resource) -> String {
    fingerprint(
        &resource.path_rel,
        resource.kind,
        resource.role,
        resource.language,
        resource.size_bytes,
        resource.content_hash.as_deref(),
        resource.generated_kind.as_deref(),
        resource.container_resource_id,
    )
}

/// Accept only unambiguous move evidence, keyed destination → source.
///
/// Every one of these must hold, or the evidence is dropped and the paths
/// fall back to independent delete + create: the source is a known ACTIVE
/// Resource, the source path is gone, the destination is observed, the
/// destination is not already an ACTIVE Resource, and neither endpoint
/// appears twice across the evidence.
fn accept_moves<'a>(
    by_path: &HashMap<&'a str, &'a Resource>,
    observed_paths: &HashSet<&str>,
    move_evidence: &'a [MoveEvidence],
) -> HashMap<&'a str, &'a str> {
    let mut source_uses: HashMap<&str, usize> = HashMap::new();
    let mut destination_uses: HashMap<&str, usize> = HashMap::new();
    for evidence in move_evidence {
        *source_uses
            .entry(evidence.from_path_key.as_str())
            .or_default() += 1;
        *destination_uses
            .entry(evidence.to_path_key.as_str())
            .or_default() += 1;
    }

    move_evidence
        .iter()
        .filter(|evidence| {
            let from = evidence.from_path_key.as_str();
            let to = evidence.to_path_key.as_str();
            by_path.contains_key(from)
                && !observed_paths.contains(from)
                && observed_paths.contains(to)
                && !by_path.contains_key(to)
                && source_uses.get(from) == Some(&1)
                && destination_uses.get(to) == Some(&1)
        })
        .map(|evidence| {
            (
                evidence.to_path_key.as_str(),
                evidence.from_path_key.as_str(),
            )
        })
        .collect()
}

fn modify(previous: &Resource, entry: &ObservedResource) -> Result<ResourceChange, IdentityError> {
    let candidate = build_resource(
        previous.id,
        previous.resource_revision.clone(),
        entry,
        previous.generated_kind.clone(),
        previous.container_resource_id,
    );

    if candidate.fingerprint != previous.fingerprint {
        return Ok(ResourceChange::Update(Resource {
            resource_revision: next_revision(&previous.resource_revision)?,
            ..candidate
        }));
    }
    if previous.size_bytes != entry.size_bytes || previous.mtime_ns != entry.mtime_ns {
        return Ok(ResourceChange::MetadataRefresh {
            id: previous.id,
            size_bytes: entry.size_bytes,
            mtime_ns: entry.mtime_ns,
        });
    }
    Ok(ResourceChange::Unchanged { id: previous.id })
}

fn build_resource(
    id: ResourceId,
    resource_revision: String,
    entry: &ObservedResource,
    generated_kind: Option<String>,
    container_resource_id: Option<ResourceId>,
) -> Resource {
    let discovered = &entry.discovered;
    Resource {
        id,
        path_rel: discovered.path_rel.clone(),
        path_key: discovered.path_key.clone(),
        kind: discovered.kind,
        role: discovered.role,
        language: discovered.language,
        size_bytes: entry.size_bytes,
        mtime_ns: entry.mtime_ns,
        fingerprint: fingerprint(
            &discovered.path_rel,
            discovered.kind,
            discovered.role,
            discovered.language,
            entry.size_bytes,
            entry.content_hash.as_deref(),
            generated_kind.as_deref(),
            container_resource_id,
        ),
        content_hash: entry.content_hash.clone(),
        state: ResourceState::Active,
        resource_revision,
        generated_kind,
        container_resource_id,
    }
}

#[allow(clippy::too_many_arguments)]
fn fingerprint(
    path_rel: &str,
    kind: ResourceKind,
    role: ResourceRole,
    language: Option<ResourceLanguage>,
    size_bytes: i64,
    content_hash: Option<&str>,
    generated_kind: Option<&str>,
    container_resource_id: Option<ResourceId>,
) -> String {
    let mut hasher = Sha256::new();
    field(&mut hasher, "path", path_rel.as_bytes());
    field(&mut hasher, "kind", kind.to_string().as_bytes());
    field(&mut hasher, "role", role.to_string().as_bytes());
    optional_field(
        &mut hasher,
        "language",
        language.map(|language| language.to_string()).as_deref(),
    );
    field(&mut hasher, "size", size_bytes.to_string().as_bytes());
    optional_field(&mut hasher, "content", content_hash);
    optional_field(&mut hasher, "generated_kind", generated_kind);
    optional_field(
        &mut hasher,
        "container",
        container_resource_id.map(|id| id.to_string()).as_deref(),
    );
    format!("{FINGERPRINT_ALGORITHM}:{:x}", hasher.finalize())
}

/// Length-prefixed field encoding: no value can be confused with a
/// different field, a different boundary, or a value containing the
/// separator.
fn field(hasher: &mut Sha256, name: &str, value: &[u8]) {
    hasher.update(name.as_bytes());
    hasher.update(b":");
    hasher.update(value.len().to_string().as_bytes());
    hasher.update(b":");
    hasher.update(value);
    hasher.update(b";");
}

/// `Some("")` and `None` must not hash alike, so presence is its own byte.
fn optional_field(hasher: &mut Sha256, name: &str, value: Option<&str>) {
    match value {
        Some(value) => {
            hasher.update(b"+");
            field(hasher, name, value.as_bytes());
        }
        None => {
            hasher.update(b"-");
            field(hasher, name, b"");
        }
    }
}

fn hash_file(path: &Path) -> Result<String, IdentityError> {
    // ponytail: whole-file read; swap for a streaming reader if Workspaces
    // with very large files show up in the task 4 scan benchmark.
    let bytes = fs::read(path).map_err(|source| io_error(path, source))?;
    let digest = Sha256::digest(&bytes);
    Ok(format!("{CONTENT_HASH_ALGORITHM}:{digest:x}"))
}

fn mtime_nanos(metadata: &fs::Metadata) -> i64 {
    // ponytail: an unreadable or pre-epoch mtime collapses to 0. mtime is
    // only ever a fast-path hint here -- collapsing it costs one extra
    // content hash, never a wrong identity or revision decision.
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .and_then(|elapsed| i64::try_from(elapsed.as_nanos()).ok())
        .unwrap_or(0)
}

fn io_error(path: &Path, source: std::io::Error) -> IdentityError {
    IdentityError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env, process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{
        config::WorkspaceConfig,
        discovery::enumerate_resources,
        resource::{ResourceRole, is_tombstone_path_key},
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    /// A Workspace root plus its own `index.db`, driven through the real
    /// discovery → observe → plan → apply path. The DB lives *outside* the
    /// Workspace root so it never shows up in the inventory itself.
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-identity-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            fs::create_dir_all(&root).expect("workspace root should be creatable");
            Self { base, root }
        }

        fn db_path(&self) -> PathBuf {
            self.base.join("data").join("index.db")
        }

        fn store(&self) -> ResourceStore {
            ResourceStore::open(&self.db_path()).expect("store should open")
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

        fn observe_all(
            &self,
            store: &ResourceStore,
            mode: ObservationMode,
        ) -> Vec<ObservedResource> {
            let discovered = enumerate_resources(&self.root, &WorkspaceConfig::default())
                .expect("enumeration should succeed");
            discovered
                .iter()
                .map(|entry| {
                    let previous = store
                        .get_active_by_path_key(&entry.path_key)
                        .expect("lookup should succeed");
                    observe(&self.root, entry, previous.as_ref(), mode)
                        .expect("observe should succeed")
                })
                .collect()
        }

        /// Default to the correctness path; the latency path is exercised
        /// explicitly where it matters.
        fn sync(&self, store: &ResourceStore) -> Vec<ResourceChange> {
            self.sync_with_moves(store, &[], ObservationMode::Verified)
        }

        fn sync_in(&self, store: &ResourceStore, mode: ObservationMode) -> Vec<ResourceChange> {
            self.sync_with_moves(store, &[], mode)
        }

        fn sync_with_moves(
            &self,
            store: &ResourceStore,
            moves: &[MoveEvidence],
            mode: ObservationMode,
        ) -> Vec<ResourceChange> {
            let active = store.list_active().expect("active list should succeed");
            let observed = self.observe_all(store, mode);
            let changes = plan_changes(&active, &observed, moves).expect("planning should succeed");
            apply(store, &changes).expect("apply should succeed");
            changes
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn active(store: &ResourceStore, path_key: &str) -> Resource {
        store
            .get_active_by_path_key(path_key)
            .expect("lookup should succeed")
            .unwrap_or_else(|| panic!("expected an ACTIVE resource at {path_key}"))
    }

    fn touch_mtime(fixture: &Fixture, rel: &str) {
        // Rewriting identical bytes is the cheapest portable way to move
        // mtime without changing content; a spin guards coarse timestamps.
        let path = fixture.root.join(rel);
        let before = fs::metadata(&path)
            .expect("metadata")
            .modified()
            .expect("mtime");
        let contents = fs::read(&path).expect("read");
        loop {
            fs::write(&path, &contents).expect("rewrite");
            let after = fs::metadata(&path)
                .expect("metadata")
                .modified()
                .expect("mtime");
            if after != before {
                return;
            }
        }
    }

    #[test]
    fn first_create_assigns_a_new_stable_id_at_the_initial_revision() {
        let fixture = Fixture::create("create");
        fixture.write("src/lib.rs", "fn main() {}");
        let store = fixture.store();

        let changes = fixture.sync(&store);

        assert!(
            changes
                .iter()
                .all(|change| matches!(change, ResourceChange::Create(_)))
        );
        let resource = active(&store, "src/lib.rs");
        assert_eq!(resource.resource_revision, INITIAL_REVISION);
        assert_eq!(resource.state, ResourceState::Active);
        assert!(resource.fingerprint.starts_with(FINGERPRINT_ALGORITHM));
        assert!(
            resource
                .content_hash
                .as_deref()
                .is_some_and(|hash| hash.starts_with(CONTENT_HASH_ALGORITHM)),
            "a file Resource must carry content evidence"
        );
    }

    #[test]
    fn an_unchanged_path_keeps_its_id_and_revision() {
        let fixture = Fixture::create("unchanged");
        fixture.write("src/lib.rs", "fn main() {}");
        let store = fixture.store();
        fixture.sync(&store);
        let first = active(&store, "src/lib.rs");

        let changes = fixture.sync(&store);

        assert!(
            changes
                .iter()
                .all(|change| matches!(change, ResourceChange::Unchanged { .. })),
            "a re-scan of an untouched Workspace must be a complete no-op"
        );
        let second = active(&store, "src/lib.rs");
        assert_eq!(second.id, first.id);
        assert_eq!(second.resource_revision, first.resource_revision);
    }

    #[test]
    fn an_mtime_only_change_does_not_advance_the_revision() {
        let fixture = Fixture::create("mtime-only");
        fixture.write("src/lib.rs", "fn main() {}");
        let store = fixture.store();
        fixture.sync(&store);
        let first = active(&store, "src/lib.rs");

        touch_mtime(&fixture, "src/lib.rs");
        let changes = fixture.sync(&store);

        assert!(
            changes
                .iter()
                .any(|change| matches!(change, ResourceChange::MetadataRefresh { .. })),
            "the mtime drift should be recorded as metadata only"
        );
        let second = active(&store, "src/lib.rs");
        assert_eq!(second.id, first.id);
        assert_eq!(second.resource_revision, first.resource_revision);
        assert_eq!(second.fingerprint, first.fingerprint);
        assert_ne!(second.mtime_ns, first.mtime_ns);
    }

    /// Rewrites `rel` with equal-length content and restores the original
    /// mtime, so the file is indistinguishable from its predecessor by
    /// metadata alone -- exactly the case a metadata-only fast path misses.
    fn overwrite_preserving_metadata(fixture: &Fixture, rel: &str, contents: &str) {
        let path = fixture.root.join(rel);
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

        let after = fs::metadata(&path).expect("metadata");
        assert_eq!(after.len(), before.len());
        assert_eq!(after.modified().expect("mtime"), mtime);
    }

    #[test]
    fn a_same_size_same_mtime_edit_is_fast_path_invisible_but_verified_catches_it() {
        let fixture = Fixture::create("stealth-edit");
        fixture.write("src/lib.rs", "fn main() { a() }");
        let store = fixture.store();
        fixture.sync(&store);
        let before = active(&store, "src/lib.rs");

        overwrite_preserving_metadata(&fixture, "src/lib.rs", "fn main() { b() }");

        // Fast is allowed to reuse the recorded hash: that is the latency
        // trade-off, and it is why it must not be the only path.
        let fast = fixture.sync_in(&store, ObservationMode::Fast);
        assert!(
            fast.iter()
                .all(|change| matches!(change, ResourceChange::Unchanged { .. })),
            "the metadata fast path cannot see this edit, by construction"
        );
        assert_eq!(active(&store, "src/lib.rs"), before);

        // Verified re-hashes the current bytes and catches it.
        let verified = fixture.sync_in(&store, ObservationMode::Verified);
        assert!(
            verified
                .iter()
                .any(|change| matches!(change, ResourceChange::Update(_))),
            "the correctness path must detect a metadata-identical edit"
        );
        let after = active(&store, "src/lib.rs");
        assert_eq!(after.id, before.id);
        assert_eq!(after.resource_revision, "2");
        assert_ne!(after.fingerprint, before.fingerprint);
        assert_ne!(after.content_hash, before.content_hash);
    }

    #[test]
    fn verified_re_hashing_of_unchanged_content_keeps_the_revision() {
        let fixture = Fixture::create("verified-noop");
        fixture.write("src/lib.rs", "fn main() {}");
        let store = fixture.store();
        fixture.sync(&store);
        let before = active(&store, "src/lib.rs");

        // Re-hashing identical bytes is still a no-op: re-verification is
        // not, by itself, a change.
        touch_mtime(&fixture, "src/lib.rs");
        fixture.sync_in(&store, ObservationMode::Verified);

        let after = active(&store, "src/lib.rs");
        assert_eq!(after.id, before.id);
        assert_eq!(after.resource_revision, before.resource_revision);
        assert_eq!(after.fingerprint, before.fingerprint);
    }

    #[test]
    fn a_content_change_keeps_the_id_and_advances_the_revision() {
        let fixture = Fixture::create("content-change");
        fixture.write("src/lib.rs", "fn main() {}");
        let store = fixture.store();
        fixture.sync(&store);
        let first = active(&store, "src/lib.rs");

        fixture.write("src/lib.rs", "fn main() { let changed = 1; }");
        fixture.sync(&store);

        let second = active(&store, "src/lib.rs");
        assert_eq!(
            second.id, first.id,
            "same path content edit must keep identity"
        );
        assert_eq!(second.resource_revision, "2");
        assert_ne!(second.fingerprint, first.fingerprint);
    }

    #[test]
    fn a_classification_change_advances_the_revision_without_a_content_change() {
        // Same bytes, same path, but role/language differ: the fingerprint
        // covers structural inputs, not just content.
        let base = ObservedResource {
            discovered: DiscoveredResource {
                path_rel: "src/app.py".to_owned(),
                path_key: "src/app.py".to_owned(),
                kind: ResourceKind::File,
                role: ResourceRole::Source,
                language: Some(ResourceLanguage::Python),
            },
            size_bytes: 10,
            mtime_ns: 1,
            content_hash: Some("sha256:abc".to_owned()),
        };
        let id = ResourceId::generate();
        let previous = build_resource(id, INITIAL_REVISION.to_owned(), &base, None, None);

        for altered in [
            ObservedResource {
                discovered: DiscoveredResource {
                    role: ResourceRole::Test,
                    ..base.discovered.clone()
                },
                ..base.clone()
            },
            ObservedResource {
                discovered: DiscoveredResource {
                    language: Some(ResourceLanguage::TypeScript),
                    ..base.discovered.clone()
                },
                ..base.clone()
            },
            ObservedResource {
                discovered: DiscoveredResource {
                    kind: ResourceKind::Directory,
                    ..base.discovered.clone()
                },
                ..base.clone()
            },
        ] {
            let change = plan_changes(std::slice::from_ref(&previous), &[altered], &[])
                .expect("planning should succeed");
            let ResourceChange::Update(updated) = &change[0] else {
                panic!("a structural input change must be an Update, got {change:?}");
            };
            assert_eq!(updated.id, id);
            assert_eq!(updated.resource_revision, "2");
        }
    }

    #[test]
    fn a_container_change_advances_the_revision() {
        let entry = ObservedResource {
            discovered: DiscoveredResource {
                path_rel: "src/app.svelte".to_owned(),
                path_key: "src/app.svelte".to_owned(),
                kind: ResourceKind::File,
                role: ResourceRole::Source,
                language: Some(ResourceLanguage::Svelte),
            },
            size_bytes: 10,
            mtime_ns: 1,
            content_hash: Some("sha256:abc".to_owned()),
        };
        let without = build_resource(
            ResourceId::generate(),
            INITIAL_REVISION.to_owned(),
            &entry,
            None,
            None,
        );
        let with = build_resource(
            without.id,
            INITIAL_REVISION.to_owned(),
            &entry,
            None,
            Some(ResourceId::generate()),
        );

        assert_ne!(
            without.fingerprint, with.fingerprint,
            "container mapping is a structural input"
        );
    }

    #[test]
    fn a_delete_tombstones_the_same_id_and_advances_the_revision() {
        let fixture = Fixture::create("delete");
        fixture.write("src/lib.rs", "fn main() {}");
        let store = fixture.store();
        fixture.sync(&store);
        let before = active(&store, "src/lib.rs");

        fixture.remove("src/lib.rs");
        fixture.sync(&store);

        let tombstone = store
            .get_by_id(before.id)
            .expect("lookup should succeed")
            .expect("the tombstone must keep the stable id");
        assert_eq!(tombstone.state, ResourceState::Deleted);
        assert_eq!(tombstone.resource_revision, "2");
        assert_eq!(
            tombstone.path_rel, "src/lib.rs",
            "the user-facing historical path must be preserved"
        );
        assert!(
            is_tombstone_path_key(&tombstone.path_key),
            "the internal key must move into the tombstone namespace"
        );
    }

    #[test]
    fn a_deleted_resource_is_excluded_from_current_lookups() {
        let fixture = Fixture::create("delete-lookup");
        fixture.write("src/lib.rs", "fn main() {}");
        let store = fixture.store();
        fixture.sync(&store);

        fixture.remove("src/lib.rs");
        fixture.sync(&store);

        assert!(
            store
                .get_active_by_path_key("src/lib.rs")
                .expect("lookup should succeed")
                .is_none()
        );
        assert!(
            !store
                .list_active()
                .expect("list should succeed")
                .iter()
                .any(|resource| resource.path_rel == "src/lib.rs")
        );
    }

    #[test]
    fn repeating_a_delete_is_a_no_op() {
        let fixture = Fixture::create("repeat-delete");
        fixture.write("src/lib.rs", "fn main() {}");
        let store = fixture.store();
        fixture.sync(&store);
        let id = active(&store, "src/lib.rs").id;

        fixture.remove("src/lib.rs");
        fixture.sync(&store);
        let after_first = store
            .get_by_id(id)
            .expect("lookup should succeed")
            .expect("tombstone should exist");

        // Replaying the same delete decision must not advance anything.
        apply(
            &store,
            &[ResourceChange::Delete {
                id,
                resource_revision: "99".to_owned(),
            }],
        )
        .expect("replayed delete should apply cleanly");

        let after_second = store
            .get_by_id(id)
            .expect("lookup should succeed")
            .expect("tombstone should still exist");
        assert_eq!(after_second, after_first);
    }

    #[test]
    fn a_same_path_recreate_gets_a_new_resource_id() {
        let fixture = Fixture::create("recreate");
        fixture.write("src/lib.rs", "fn main() {}");
        let store = fixture.store();
        fixture.sync(&store);
        let original = active(&store, "src/lib.rs").id;

        fixture.remove("src/lib.rs");
        fixture.sync(&store);
        fixture.write("src/lib.rs", "totally unrelated content");
        fixture.sync(&store);

        let recreated = active(&store, "src/lib.rs");
        assert_ne!(
            recreated.id, original,
            "a tombstone must never be resurrected without continuity evidence"
        );
        assert_eq!(recreated.resource_revision, INITIAL_REVISION);
        assert_eq!(
            store
                .get_by_id(original)
                .expect("lookup should succeed")
                .expect("the tombstone must survive")
                .state,
            ResourceState::Deleted
        );
    }

    #[test]
    fn active_path_uniqueness_survives_delete_and_recreate() {
        let fixture = Fixture::create("active-uniqueness");
        fixture.write("src/lib.rs", "one");
        let store = fixture.store();
        fixture.sync(&store);
        fixture.remove("src/lib.rs");
        fixture.sync(&store);
        fixture.write("src/lib.rs", "two");
        fixture.sync(&store);

        let active_at_path: Vec<_> = store
            .list_active()
            .expect("list should succeed")
            .into_iter()
            .filter(|resource| resource.path_key == "src/lib.rs")
            .collect();
        assert_eq!(
            active_at_path.len(),
            1,
            "exactly one ACTIVE Resource may hold a path"
        );
        assert_eq!(
            store.list().expect("list should succeed").len(),
            3,
            "both tombstones and the current Resource must be retained (src/ + 2 files)"
        );
    }

    #[test]
    fn explicit_move_evidence_keeps_the_id_and_advances_the_revision() {
        let fixture = Fixture::create("move");
        fixture.write("src/lib.rs", "fn main() {}");
        let store = fixture.store();
        fixture.sync(&store);
        let before = active(&store, "src/lib.rs");

        fixture.remove("src/lib.rs");
        fixture.write("src/renamed.rs", "fn main() {}");
        fixture.sync_with_moves(
            &store,
            &[MoveEvidence {
                from_path_key: "src/lib.rs".to_owned(),
                to_path_key: "src/renamed.rs".to_owned(),
            }],
            ObservationMode::Verified,
        );

        let moved = active(&store, "src/renamed.rs");
        assert_eq!(moved.id, before.id, "evidenced move must keep identity");
        assert_eq!(moved.path_rel, "src/renamed.rs");
        assert_eq!(
            moved.resource_revision, "2",
            "a path change is itself a semantic input change"
        );
        assert!(
            store
                .get_active_by_path_key("src/lib.rs")
                .expect("lookup should succeed")
                .is_none()
        );
    }

    #[test]
    fn identical_content_alone_is_never_treated_as_a_move() {
        let fixture = Fixture::create("copy-delete");
        fixture.write("src/lib.rs", "fn main() {}");
        let store = fixture.store();
        fixture.sync(&store);
        let before = active(&store, "src/lib.rs");

        // Copy + delete with no evidence: ambiguous, so prefer a split.
        fixture.write("src/copy.rs", "fn main() {}");
        fixture.remove("src/lib.rs");
        fixture.sync(&store);

        let created = active(&store, "src/copy.rs");
        assert_ne!(
            created.id, before.id,
            "identical content is not move evidence"
        );
        assert_eq!(created.resource_revision, INITIAL_REVISION);
        assert_eq!(
            store
                .get_by_id(before.id)
                .expect("lookup should succeed")
                .expect("old resource should still exist")
                .state,
            ResourceState::Deleted
        );
    }

    #[test]
    fn duplicate_identical_files_keep_separate_identities() {
        let fixture = Fixture::create("duplicates");
        fixture.write("a.rs", "fn main() {}");
        fixture.write("b.rs", "fn main() {}");
        let store = fixture.store();
        fixture.sync(&store);

        let first = active(&store, "a.rs");
        let second = active(&store, "b.rs");
        assert_ne!(first.id, second.id);
        assert_eq!(
            first.content_hash, second.content_hash,
            "identical bytes must still hash identically"
        );

        // Editing one must not disturb the other.
        fixture.write("a.rs", "fn main() { let edited = 1; }");
        fixture.sync(&store);
        assert_eq!(active(&store, "a.rs").id, first.id);
        assert_eq!(active(&store, "b.rs"), second);
    }

    #[test]
    fn ambiguous_move_evidence_is_rejected_in_favour_of_delete_and_create() {
        let fixture = Fixture::create("ambiguous-move");
        fixture.write("src/lib.rs", "fn main() {}");
        let store = fixture.store();
        fixture.sync(&store);
        let before = active(&store, "src/lib.rs");

        fixture.remove("src/lib.rs");
        fixture.write("src/one.rs", "fn main() {}");
        fixture.write("src/two.rs", "fn main() {}");
        // One source claimed by two destinations: not high-confidence.
        fixture.sync_with_moves(
            &store,
            &[
                MoveEvidence {
                    from_path_key: "src/lib.rs".to_owned(),
                    to_path_key: "src/one.rs".to_owned(),
                },
                MoveEvidence {
                    from_path_key: "src/lib.rs".to_owned(),
                    to_path_key: "src/two.rs".to_owned(),
                },
            ],
            ObservationMode::Verified,
        );

        assert_ne!(active(&store, "src/one.rs").id, before.id);
        assert_ne!(active(&store, "src/two.rs").id, before.id);
        assert_eq!(
            store
                .get_by_id(before.id)
                .expect("lookup should succeed")
                .expect("old resource should still exist")
                .state,
            ResourceState::Deleted
        );
    }

    #[test]
    fn move_evidence_is_rejected_when_the_source_path_still_exists() {
        let fixture = Fixture::create("move-source-alive");
        fixture.write("src/lib.rs", "fn main() {}");
        let store = fixture.store();
        fixture.sync(&store);
        let before = active(&store, "src/lib.rs");

        fixture.write("src/copy.rs", "fn main() {}");
        fixture.sync_with_moves(
            &store,
            &[MoveEvidence {
                from_path_key: "src/lib.rs".to_owned(),
                to_path_key: "src/copy.rs".to_owned(),
            }],
            ObservationMode::Verified,
        );

        assert_eq!(active(&store, "src/lib.rs").id, before.id);
        assert_ne!(active(&store, "src/copy.rs").id, before.id);
    }

    #[test]
    fn ids_and_revisions_survive_a_db_reopen() {
        let fixture = Fixture::create("reopen");
        fixture.write("src/lib.rs", "fn main() {}");
        let before = {
            let store = fixture.store();
            fixture.sync(&store);
            fixture.write("src/lib.rs", "fn main() { let edited = 1; }");
            fixture.sync(&store);
            active(&store, "src/lib.rs")
        };
        assert_eq!(before.resource_revision, "2");

        let reopened = fixture.store();
        let after = active(&reopened, "src/lib.rs");
        assert_eq!(after, before);

        // A re-scan against the reopened DB is still a no-op.
        let changes = fixture.sync(&reopened);
        assert!(
            changes
                .iter()
                .all(|change| matches!(change, ResourceChange::Unchanged { .. }))
        );
    }

    #[test]
    fn separate_worktree_dbs_never_share_identity() {
        let first = Fixture::create("worktree-a");
        let second = Fixture::create("worktree-b");
        first.write("src/lib.rs", "fn main() {}");
        second.write("src/lib.rs", "fn main() {}");

        let first_store = first.store();
        let second_store = second.store();
        first.sync(&first_store);
        second.sync(&second_store);

        assert_ne!(
            active(&first_store, "src/lib.rs").id,
            active(&second_store, "src/lib.rs").id,
            "identical paths in separate Workspaces are separate Resources"
        );
        assert_eq!(
            second_store
                .list_active()
                .expect("list should succeed")
                .len(),
            2,
            "one Workspace's inventory must not leak into another's DB"
        );
    }

    #[test]
    fn a_directory_revision_does_not_follow_its_children() {
        let fixture = Fixture::create("directory-revision");
        fixture.write("src/lib.rs", "fn main() {}");
        let store = fixture.store();
        fixture.sync(&store);
        let directory = active(&store, "src");
        assert_eq!(directory.content_hash, None);

        fixture.write("src/lib.rs", "fn main() { let edited = 1; }");
        fixture.write("src/added.rs", "fn added() {}");
        fixture.sync(&store);

        let after = active(&store, "src");
        assert_eq!(after.resource_revision, directory.resource_revision);
        assert_eq!(after.fingerprint, directory.fingerprint);
    }

    #[test]
    fn the_fingerprint_is_deterministic_and_ignores_mtime() {
        let entry = ObservedResource {
            discovered: DiscoveredResource {
                path_rel: "src/lib.rs".to_owned(),
                path_key: "src/lib.rs".to_owned(),
                kind: ResourceKind::File,
                role: ResourceRole::Source,
                language: Some(ResourceLanguage::Rust),
            },
            size_bytes: 12,
            mtime_ns: 1,
            content_hash: Some("sha256:abc".to_owned()),
        };
        let id = ResourceId::generate();
        let first = build_resource(id, INITIAL_REVISION.to_owned(), &entry, None, None);
        let later = build_resource(
            id,
            INITIAL_REVISION.to_owned(),
            &ObservedResource {
                mtime_ns: 999_999,
                ..entry
            },
            None,
            None,
        );

        assert_eq!(first.fingerprint, later.fingerprint);
        assert_eq!(fingerprint_of(&first), first.fingerprint);
    }

    #[test]
    fn a_non_decimal_revision_is_rejected_rather_than_restarted() {
        let error =
            next_revision("rev-1").expect_err("legacy revision text must not silently reset");
        assert!(matches!(
            error,
            IdentityError::UnreadableRevision { raw } if raw == "rev-1"
        ));
    }
}
