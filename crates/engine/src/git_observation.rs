//! Caller-observed Git state → the #20 task 3 observation types (#50,
//! I6 task 1).
//!
//! Pure conversion: no command runs here, and nothing is checked against
//! the filesystem or Git. The caller observed the entries; Brainprint
//! stores them as observed, fingerprints them deterministically, and maps
//! each path onto an ACTIVE Resource only when index.db already has one.

use std::{collections::BTreeSet, fmt};

use brainprint_core::ResourceId;
use sha2::{Digest, Sha256};

use crate::{
    identity::{field, optional_field},
    knowledge::{DirtyObservation, ResourceObservation},
    resource::{ResourceError, ResourceStore},
};

/// Entries per observation: the #20 task 3 pre-existing dirty bound.
pub const GIT_ENTRY_BOUND: usize = 512;

const FINGERPRINT_TAG: &str = "git-entries-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GitEntryStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

impl GitEntryStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Untracked => "untracked",
            Self::Conflicted => "conflicted",
        }
    }
}

/// One observed change. `path` is Workspace-root relative with `/`
/// separators, as Git reports it. `old_path` is set exactly for renames.
/// Field order is the canonical sort order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GitEntry {
    pub path: String,
    pub old_path: Option<String>,
    pub status: GitEntryStatus,
}

/// What the caller saw. UNKNOWN (did not look) and CLEAN (looked, nothing
/// changed) never merge; DIRTY lists at least one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitObservation {
    Unknown,
    Clean,
    Dirty(Vec<GitEntry>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitObservationError {
    EmptyEntries,
    TooManyEntries { count: usize },
    InvalidPath { path: String, reason: &'static str },
    RenameWithoutOldPath { path: String },
    OldPathWithoutRename { path: String },
}

impl fmt::Display for GitObservationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyEntries => formatter.write_str("a DIRTY observation lists no entries"),
            Self::TooManyEntries { count } => write!(
                formatter,
                "{count} entries exceed the bound of {GIT_ENTRY_BOUND}"
            ),
            Self::InvalidPath { path, reason } => write!(formatter, "path {path:?}: {reason}"),
            Self::RenameWithoutOldPath { path } => {
                write!(formatter, "renamed entry {path:?} has no old_path")
            }
            Self::OldPathWithoutRename { path } => {
                write!(
                    formatter,
                    "entry {path:?} has an old_path but is not renamed"
                )
            }
        }
    }
}

impl std::error::Error for GitObservationError {}

/// Validated, trailing-slash-trimmed, sorted and deduplicated entries.
/// Rejects the whole list on the first invalid entry.
pub fn canonical_entries(entries: &[GitEntry]) -> Result<Vec<GitEntry>, GitObservationError> {
    if entries.is_empty() {
        return Err(GitObservationError::EmptyEntries);
    }
    if entries.len() > GIT_ENTRY_BOUND {
        return Err(GitObservationError::TooManyEntries {
            count: entries.len(),
        });
    }
    let mut canonical = BTreeSet::new();
    for entry in entries {
        let path = normalized_path(&entry.path)?;
        let old_path = match (entry.status, &entry.old_path) {
            (GitEntryStatus::Renamed, Some(old)) => Some(normalized_path(old)?),
            (GitEntryStatus::Renamed, None) => {
                return Err(GitObservationError::RenameWithoutOldPath { path });
            }
            (_, Some(_)) => return Err(GitObservationError::OldPathWithoutRename { path }),
            (_, None) => None,
        };
        canonical.insert(GitEntry {
            path,
            old_path,
            status: entry.status,
        });
    }
    Ok(canonical.into_iter().collect())
}

/// Order- and duplicate-independent fingerprint of canonical entries.
#[must_use]
pub fn fingerprint(canonical: &[GitEntry]) -> String {
    let mut hasher = Sha256::new();
    field(&mut hasher, "tag", FINGERPRINT_TAG.as_bytes());
    for entry in canonical {
        field(&mut hasher, "status", entry.status.as_str().as_bytes());
        field(&mut hasher, "path", entry.path.as_bytes());
        optional_field(&mut hasher, "old_path", entry.old_path.as_deref());
    }
    format!("sha256:{:x}", hasher.finalize())
}

/// Canonical entries plus the #20 task 3 dirty observation they imply.
pub fn dirty_observation(
    observation: &GitObservation,
) -> Result<(DirtyObservation, Vec<GitEntry>), GitObservationError> {
    Ok(match observation {
        GitObservation::Unknown => (DirtyObservation::Unknown, Vec::new()),
        GitObservation::Clean => (DirtyObservation::Clean, Vec::new()),
        GitObservation::Dirty(entries) => {
            let canonical = canonical_entries(entries)?;
            let fingerprint = fingerprint(&canonical);
            (DirtyObservation::Dirty { fingerprint }, canonical)
        }
    })
}

/// Paths mapped onto index.db's ACTIVE Resources. Nothing is created for
/// a path the index does not hold as ACTIVE; it is reported instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedPaths {
    pub resources: Vec<ResourceObservation>,
    pub unresolved: Vec<String>,
}

/// Look up each canonical entry's `path` (a rename's new path). One
/// indexed `path_key` lookup per entry, at most [`GIT_ENTRY_BOUND`].
pub fn resolve_active(
    store: &ResourceStore,
    canonical: &[GitEntry],
) -> Result<ResolvedPaths, ResourceError> {
    let mut resolved = ResolvedPaths::default();
    let mut seen_resources: BTreeSet<ResourceId> = BTreeSet::new();
    let mut seen_paths: BTreeSet<&str> = BTreeSet::new();
    for entry in canonical {
        if !seen_paths.insert(&entry.path) {
            continue;
        }
        match store.get_active_by_path_key(&entry.path)? {
            Some(resource) => {
                if seen_resources.insert(resource.id) {
                    resolved.resources.push(ResourceObservation {
                        resource: resource.id,
                        locator_hint: Some(entry.path.clone()),
                    });
                }
            }
            None => resolved.unresolved.push(entry.path.clone()),
        }
    }
    Ok(resolved)
}

/// Workspace-relative, `/`-separated, no `.`/`..`/empty component. One
/// trailing `/` (Git's untracked-directory form) is trimmed.
fn normalized_path(path: &str) -> Result<String, GitObservationError> {
    let invalid = |reason| GitObservationError::InvalidPath {
        path: path.to_owned(),
        reason,
    };
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    if trimmed.is_empty() {
        return Err(invalid("empty"));
    }
    if trimmed.contains('\0') {
        return Err(invalid("contains NUL"));
    }
    let bytes = trimmed.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if trimmed.starts_with(['/', '\\']) || drive {
        return Err(invalid("absolute; must be Workspace-root relative"));
    }
    for component in trimmed.split(['/', '\\']) {
        match component {
            "" => return Err(invalid("empty path component")),
            "." | ".." => return Err(invalid("`.`/`..` component")),
            _ => {}
        }
    }
    Ok(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(status: GitEntryStatus, path: &str) -> GitEntry {
        GitEntry {
            path: path.to_owned(),
            old_path: None,
            status,
        }
    }

    fn renamed(old: &str, new: &str) -> GitEntry {
        GitEntry {
            path: new.to_owned(),
            old_path: Some(old.to_owned()),
            status: GitEntryStatus::Renamed,
        }
    }

    fn print(entries: &[GitEntry]) -> String {
        fingerprint(&canonical_entries(entries).expect("valid"))
    }

    #[test]
    fn fingerprint_ignores_order_and_duplicates() {
        let a = entry(GitEntryStatus::Modified, "src/a.rs");
        let b = entry(GitEntryStatus::Untracked, "notes/");
        let c = renamed("old.rs", "new.rs");
        let forward = print(&[a.clone(), b.clone(), c.clone()]);
        assert_eq!(forward, print(&[c.clone(), a.clone(), b.clone()]));
        assert_eq!(forward, print(&[a.clone(), b, c, a]));
        assert!(forward.starts_with("sha256:"));
    }

    #[test]
    fn fingerprint_changes_with_status_path_or_old_path() {
        let base = print(&[entry(GitEntryStatus::Modified, "a.rs")]);
        assert_ne!(base, print(&[entry(GitEntryStatus::Added, "a.rs")]));
        assert_ne!(base, print(&[entry(GitEntryStatus::Modified, "b.rs")]));
        assert_ne!(
            print(&[renamed("x.rs", "a.rs")]),
            print(&[renamed("y.rs", "a.rs")])
        );
        // A field boundary cannot be shifted to fake another entry list.
        assert_ne!(
            print(&[entry(GitEntryStatus::Modified, "ab")]),
            print(&[
                entry(GitEntryStatus::Modified, "a"),
                entry(GitEntryStatus::Modified, "b")
            ])
        );
    }

    #[test]
    fn a_trailing_slash_is_the_same_path() {
        assert_eq!(
            print(&[entry(GitEntryStatus::Untracked, "dir/")]),
            print(&[entry(GitEntryStatus::Untracked, "dir")])
        );
    }

    #[test]
    fn unknown_and_clean_stay_distinct_and_carry_no_fingerprint() {
        let (unknown, none) = dirty_observation(&GitObservation::Unknown).expect("unknown");
        let (clean, also_none) = dirty_observation(&GitObservation::Clean).expect("clean");
        assert_eq!(unknown, DirtyObservation::Unknown);
        assert_eq!(clean, DirtyObservation::Clean);
        assert_ne!(unknown, clean);
        assert_eq!(unknown.fingerprint(), None);
        assert_eq!(clean.fingerprint(), None);
        assert!(none.is_empty() && also_none.is_empty());
        let (dirty, canonical) = dirty_observation(&GitObservation::Dirty(vec![entry(
            GitEntryStatus::Modified,
            "a.rs",
        )]))
        .expect("dirty");
        assert_eq!(dirty.fingerprint(), Some(fingerprint(&canonical).as_str()));
    }

    #[test]
    fn invalid_entries_are_rejected() {
        let rejected = |entries: Vec<GitEntry>| {
            dirty_observation(&GitObservation::Dirty(entries)).expect_err("must be rejected")
        };
        assert_eq!(rejected(Vec::new()), GitObservationError::EmptyEntries);
        for path in [
            "",
            "/",
            "/etc/passwd",
            "\\x",
            "C:\\x",
            "c:/x",
            "a/../b",
            "../a",
            "./a",
            "a//b",
            "a\0b",
        ] {
            assert!(
                matches!(
                    rejected(vec![entry(GitEntryStatus::Modified, path)]),
                    GitObservationError::InvalidPath { .. }
                ),
                "{path:?} must be rejected"
            );
        }
        assert!(matches!(
            rejected(vec![GitEntry {
                path: "a".to_owned(),
                old_path: None,
                status: GitEntryStatus::Renamed,
            }]),
            GitObservationError::RenameWithoutOldPath { .. }
        ));
        assert!(matches!(
            rejected(vec![GitEntry {
                path: "a".to_owned(),
                old_path: Some("b".to_owned()),
                status: GitEntryStatus::Modified,
            }]),
            GitObservationError::OldPathWithoutRename { .. }
        ));
        assert!(matches!(
            rejected(vec![renamed("../escape", "a")]),
            GitObservationError::InvalidPath { .. }
        ));
    }

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn create(label: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "brainprint-git-observation-{label}-{}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("test directory");
            Self(path)
        }

        fn db_path(&self) -> std::path::PathBuf {
            self.0.join("data").join("index.db")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn resource(path: &str, state: crate::resource::ResourceState) -> crate::resource::Resource {
        use crate::resource::{Resource, ResourceKind, ResourceLanguage, ResourceRole};
        Resource {
            id: ResourceId::generate(),
            path_rel: path.to_owned(),
            path_key: path.to_owned(),
            kind: ResourceKind::File,
            role: ResourceRole::Source,
            language: Some(ResourceLanguage::Rust),
            size_bytes: 1,
            mtime_ns: 1,
            fingerprint: "fp".to_owned(),
            content_hash: None,
            state,
            resource_revision: "rev".to_owned(),
            generated_kind: None,
            container_resource_id: None,
        }
    }

    #[test]
    fn only_active_resources_resolve_and_nothing_is_created() {
        use crate::resource::ResourceState;
        let dir = TestDir::create("resolve");
        let store = ResourceStore::open(&dir.db_path()).expect("index.db");
        let active = resource("src/a.rs", ResourceState::Active);
        let deleted = resource("src/gone.rs", ResourceState::Deleted);
        store.insert_resource(&active).expect("insert active");
        store.insert_resource(&deleted).expect("insert deleted");
        let before = store.list().expect("list").len();

        let canonical = canonical_entries(&[
            entry(GitEntryStatus::Modified, "src/a.rs"),
            entry(GitEntryStatus::Deleted, "src/gone.rs"),
            entry(GitEntryStatus::Untracked, "scratch/"),
            renamed("src/a.rs", "src/b.rs"),
            // Same Resource twice (two statuses): recorded once.
            entry(GitEntryStatus::Conflicted, "src/a.rs"),
        ])
        .expect("valid");
        let resolved = resolve_active(&store, &canonical).expect("resolve");

        assert_eq!(
            resolved.resources,
            vec![ResourceObservation {
                resource: active.id,
                locator_hint: Some("src/a.rs".to_owned()),
            }]
        );
        assert_eq!(
            resolved.unresolved,
            vec!["scratch", "src/b.rs", "src/gone.rs"]
        );
        assert_eq!(store.list().expect("list").len(), before);
    }

    /// #12 SQL rule: the per-entry lookup is one unique-index SEARCH on
    /// `path_key`, never a scan of `resource`.
    #[test]
    fn the_path_lookup_searches_the_path_key_index() {
        let dir = TestDir::create("eqp");
        drop(ResourceStore::open(&dir.db_path()).expect("index.db"));
        let connection = rusqlite::Connection::open(dir.db_path()).expect("connection");
        let sql = format!(
            "EXPLAIN QUERY PLAN {} WHERE r.path_key = ?1 AND r.state = 'ACTIVE'",
            crate::resource::SELECT_RESOURCE_SQL
        );
        let mut statement = connection.prepare(&sql).expect("prepare");
        let plan: Vec<String> = statement
            .query_map(["src/a.rs"], |row| row.get::<_, String>(3))
            .expect("plan")
            .collect::<Result<_, _>>()
            .expect("rows");
        let text = plan.join("\n");
        println!("{text}");
        assert!(
            text.contains("SEARCH r USING INDEX") && text.contains("(path_key=?)"),
            "{text}"
        );
        assert!(!text.contains("SCAN r"), "{text}");
        assert!(!text.contains("TEMP B-TREE"), "{text}");
    }

    #[test]
    fn the_bound_is_enforced_without_truncation() {
        let at_bound: Vec<_> = (0..GIT_ENTRY_BOUND)
            .map(|index| entry(GitEntryStatus::Modified, &format!("f{index}")))
            .collect();
        assert_eq!(
            canonical_entries(&at_bound).expect("at bound").len(),
            GIT_ENTRY_BOUND
        );
        let mut over = at_bound;
        over.push(entry(GitEntryStatus::Modified, "one-more"));
        assert_eq!(
            canonical_entries(&over),
            Err(GitObservationError::TooManyEntries {
                count: GIT_ENTRY_BOUND + 1
            })
        );
    }
}
