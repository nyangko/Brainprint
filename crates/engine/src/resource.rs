//! `index.db` Resource canonical runtime model + storage boundary
//! (#16 task 1 / #13 task 6 §6).
//!
//! Scope: this module owns only the typed read/write boundary over the
//! `resource` table columns already created by #15 task 5's schema
//! ([`crate::schema::index`]) -- it defines no new columns and redesigns
//! none of the confirmed physical schema. It has no knowledge of:
//! - filesystem enumeration or ignore/exclusion policy (#16 task 2)
//! - fingerprint/revision computation or change detection (#16 task 3)
//! - create/modify/delete/move identity decisions (#16 task 3)
//! - initial scan, watcher, or reconcile (#16 task 4-6)
//!
//! `kind`/`role`/`language`/`state` are closed, explicitly-validated
//! vocabularies: an unrecognized stored value is a decode error, never a
//! silent fallback to another meaning. `id` is always the 16-byte stable
//! [`ResourceId`]; the local SQLite `resource.id` row id is an
//! implementation detail this module never exposes externally --
//! `container_resource_id` is translated to/from the container's stable
//! id at the storage boundary.

use std::{error::Error, fmt, path::Path};

use brainprint_core::ResourceId;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::{db::DbOpenError, schema};

/// Internal `path_key` namespace a DELETED tombstone is moved into.
///
/// `resource.path_key` is UNIQUE, which would otherwise make a preserved
/// DELETED tombstone block a brand-new Resource at the same path -- and the
/// only way to "resolve" that without a namespace would be to resurrect the
/// old `ResourceId`, which #16 task 3 explicitly forbids. Moving the
/// tombstone's *internal* key into this namespace keeps ACTIVE path
/// uniqueness enforced by the same UNIQUE constraint while leaving the
/// user-facing historical [`Resource::path_rel`] and the stable
/// [`ResourceId`] untouched.
///
/// The leading `/` is what makes the namespace collision-free: a discovered
/// `path_key` is always Workspace-root-relative and never starts with `/`
/// (see [`crate::discovery`]).
pub const TOMBSTONE_PATH_KEY_PREFIX: &str = "/tombstone/";

/// Whether `path_key` is an internal tombstone key rather than a real
/// current Workspace path. Never confuse it with [`Resource::path_rel`].
#[must_use]
pub fn is_tombstone_path_key(path_key: &str) -> bool {
    path_key.starts_with(TOMBSTONE_PATH_KEY_PREFIX)
}

/// Physical filesystem kind of a Resource (#13 task 6 §6 `kind` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    File,
    Directory,
}

impl ResourceKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "FILE",
            Self::Directory => "DIRECTORY",
        }
    }

    fn parse(raw: &str) -> Result<Self, ResourceError> {
        match raw {
            "FILE" => Ok(Self::File),
            "DIRECTORY" => Ok(Self::Directory),
            other => Err(ResourceError::UnknownResourceKind {
                raw: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// P0 semantic classification of a Resource (#16 task 1 §"P0 role 최소").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceRole {
    Source,
    Test,
    Config,
    Docs,
    Generated,
    Dependency,
    Asset,
    ToolState,
    Unknown,
}

impl ResourceRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::Source => "SOURCE",
            Self::Test => "TEST",
            Self::Config => "CONFIG",
            Self::Docs => "DOCS",
            Self::Generated => "GENERATED",
            Self::Dependency => "DEPENDENCY",
            Self::Asset => "ASSET",
            Self::ToolState => "TOOL_STATE",
            Self::Unknown => "UNKNOWN",
        }
    }

    /// Decode a stored role. For a reader outside this module that
    /// must respect the Resource model's classification rather than
    /// make its own (#17 task 11).
    pub fn parse_public(raw: &str) -> Result<Self, ResourceError> {
        Self::parse(raw)
    }

    fn parse(raw: &str) -> Result<Self, ResourceError> {
        match raw {
            "SOURCE" => Ok(Self::Source),
            "TEST" => Ok(Self::Test),
            "CONFIG" => Ok(Self::Config),
            "DOCS" => Ok(Self::Docs),
            "GENERATED" => Ok(Self::Generated),
            "DEPENDENCY" => Ok(Self::Dependency),
            "ASSET" => Ok(Self::Asset),
            "TOOL_STATE" => Ok(Self::ToolState),
            "UNKNOWN" => Ok(Self::Unknown),
            other => Err(ResourceError::UnknownResourceRole {
                raw: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for ResourceRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// P0 Tree-sitter target language (#16 task 1 §"Tree-sitter" target list).
/// `None` (the column is nullable) means "no programming language applies"
/// -- e.g. docs/config/asset roles -- not "unknown language".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceLanguage {
    Python,
    JavaScript,
    TypeScript,
    Svelte,
    CSharp,
    Rust,
}

impl ResourceLanguage {
    fn as_str(self) -> &'static str {
        match self {
            Self::Python => "PYTHON",
            Self::JavaScript => "JAVASCRIPT",
            Self::TypeScript => "TYPESCRIPT",
            Self::Svelte => "SVELTE",
            Self::CSharp => "CSHARP",
            Self::Rust => "RUST",
        }
    }

    fn parse(raw: &str) -> Result<Self, ResourceError> {
        match raw {
            "PYTHON" => Ok(Self::Python),
            "JAVASCRIPT" => Ok(Self::JavaScript),
            "TYPESCRIPT" => Ok(Self::TypeScript),
            "SVELTE" => Ok(Self::Svelte),
            "CSHARP" => Ok(Self::CSharp),
            "RUST" => Ok(Self::Rust),
            other => Err(ResourceError::UnknownResourceLanguage {
                raw: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for ResourceLanguage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Whether a Resource is part of the current Workspace inventory (#16 task
/// 1 completion criterion: "deleted/moved resource가 stale current result로
/// 계속 노출되지 않는다"). The actual create/modify/delete/move transition
/// rules are #16 task 3's job; this module only stores and round-trips
/// whichever state a caller supplies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceState {
    Active,
    Deleted,
}

impl ResourceState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "ACTIVE",
            Self::Deleted => "DELETED",
        }
    }

    pub(crate) fn parse(raw: &str) -> Result<Self, ResourceError> {
        match raw {
            "ACTIVE" => Ok(Self::Active),
            "DELETED" => Ok(Self::Deleted),
            other => Err(ResourceError::UnknownResourceState {
                raw: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for ResourceState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One `resource` row, keyed by its stable [`ResourceId`] -- never by
/// `path_rel`/`path_key` alone (#16 task 1: "path가 Resource identity가
/// 되지 않게 한다"). `path_rel` is the current Workspace-root-relative
/// path; no absolute source path is stored (#16 task 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub id: ResourceId,
    pub path_rel: String,
    pub path_key: String,
    pub kind: ResourceKind,
    pub role: ResourceRole,
    pub language: Option<ResourceLanguage>,
    pub size_bytes: i64,
    pub mtime_ns: i64,
    pub fingerprint: String,
    pub content_hash: Option<String>,
    pub state: ResourceState,
    pub resource_revision: String,
    /// Free-form generation-provenance tag (e.g. build output vs. authored
    /// source that happens to be checked in generated form). Orthogonal to
    /// `role`: a `Source`-role file can still carry a `generated_kind`
    /// (#16 task 1: "dependency/generated/resource role을 서로 혼동하지
    /// 않는다"). No closed vocabulary is confirmed yet, so this is stored
    /// as opaque text rather than an invented enum.
    pub generated_kind: Option<String>,
    /// The container Resource's stable id, if this Resource is owned by
    /// another Resource (e.g. a generated/mapped artifact). Never the raw
    /// local row id.
    pub container_resource_id: Option<ResourceId>,
}

/// Failure opening, decoding, or writing through the Resource storage
/// boundary.
#[derive(Debug)]
pub enum ResourceError {
    Open(DbOpenError),
    Sqlite(rusqlite::Error),
    UnknownResourceKind {
        raw: String,
    },
    UnknownResourceRole {
        raw: String,
    },
    UnknownResourceLanguage {
        raw: String,
    },
    UnknownResourceState {
        raw: String,
    },
    /// `container_resource_id` referenced a Resource id that does not
    /// exist in this `index.db`.
    UnknownContainer {
        container_id: ResourceId,
    },
}

impl fmt::Display for ResourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(source) => write!(formatter, "failed to open index.db: {source}"),
            Self::Sqlite(source) => write!(formatter, "resource store sqlite error: {source}"),
            Self::UnknownResourceKind { raw } => {
                write!(formatter, "unknown resource kind {raw:?}")
            }
            Self::UnknownResourceRole { raw } => {
                write!(formatter, "unknown resource role {raw:?}")
            }
            Self::UnknownResourceLanguage { raw } => {
                write!(formatter, "unknown resource language {raw:?}")
            }
            Self::UnknownResourceState { raw } => {
                write!(formatter, "unknown resource state {raw:?}")
            }
            Self::UnknownContainer { container_id } => write!(
                formatter,
                "container_resource_id {container_id} does not exist in this index.db"
            ),
        }
    }
}

impl Error for ResourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Open(source) => Some(source),
            Self::Sqlite(source) => Some(source),
            Self::UnknownResourceKind { .. }
            | Self::UnknownResourceRole { .. }
            | Self::UnknownResourceLanguage { .. }
            | Self::UnknownResourceState { .. }
            | Self::UnknownContainer { .. } => None,
        }
    }
}

impl From<DbOpenError> for ResourceError {
    fn from(source: DbOpenError) -> Self {
        Self::Open(source)
    }
}

impl From<rusqlite::Error> for ResourceError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

pub(crate) const SELECT_RESOURCE_SQL: &str = "
    SELECT r.uid, r.path_rel, r.path_key, r.kind, r.role, r.language, r.size, \
           r.mtime_ns, r.fingerprint, r.content_hash, r.state, r.resource_revision, \
           r.generated_kind, c.uid \
    FROM resource r \
    LEFT JOIN resource c ON r.container_resource_id = c.id";

/// Handle to one Workspace's `index.db` `resource` table.
pub struct ResourceStore {
    connection: Connection,
}

impl ResourceStore {
    /// Open (creating/migrating if needed) the `index.db` at `path`.
    pub fn open(path: &Path) -> Result<Self, ResourceError> {
        let opened = schema::index::open(path)?;
        Ok(Self::from_connection(opened.connection))
    }

    /// Wrap an already-opened `index.db` connection, mirroring
    /// [`crate::generation::GenerationStore::from_connection`] so a caller
    /// that already opened/verified `index.db` can reuse the connection.
    #[must_use]
    pub fn from_connection(connection: Connection) -> Self {
        Self { connection }
    }

    /// Persist a new Resource row. Fails if `resource.id` or
    /// `resource.path_key` already exist (unique constraints), or if
    /// `container_resource_id` names a Resource this `index.db` does not
    /// have.
    pub fn insert_resource(&self, resource: &Resource) -> Result<(), ResourceError> {
        let container_local_id = resource
            .container_resource_id
            .map(|container_id| {
                self.local_id(container_id)?
                    .ok_or(ResourceError::UnknownContainer { container_id })
            })
            .transpose()?;

        self.connection.execute(
            "INSERT INTO resource \
             (uid, path_rel, path_key, kind, role, language, size, mtime_ns, \
              fingerprint, content_hash, state, resource_revision, generated_kind, \
              container_resource_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                resource.id.to_bytes().to_vec(),
                resource.path_rel,
                resource.path_key,
                resource.kind.as_str(),
                resource.role.as_str(),
                resource.language.map(ResourceLanguage::as_str),
                resource.size_bytes,
                resource.mtime_ns,
                resource.fingerprint,
                resource.content_hash,
                resource.state.as_str(),
                resource.resource_revision,
                resource.generated_kind,
                container_local_id,
            ],
        )?;
        Ok(())
    }

    /// Look up a Resource by its stable id.
    pub fn get_by_id(&self, id: ResourceId) -> Result<Option<Resource>, ResourceError> {
        self.connection
            .query_row(
                &format!("{SELECT_RESOURCE_SQL} WHERE r.uid = ?1"),
                params![id.to_bytes().to_vec()],
                raw_resource_from_row,
            )
            .optional()?
            .map(decode_resource)
            .transpose()
    }

    /// Look up a Resource by its filesystem-scoped `path_key`.
    pub fn get_by_path_key(&self, path_key: &str) -> Result<Option<Resource>, ResourceError> {
        self.connection
            .query_row(
                &format!("{SELECT_RESOURCE_SQL} WHERE r.path_key = ?1"),
                params![path_key],
                raw_resource_from_row,
            )
            .optional()?
            .map(decode_resource)
            .transpose()
    }

    /// Look up the ACTIVE Resource currently occupying `path_key`. DELETED
    /// tombstones are never returned (#16 task 3: a deleted Resource must
    /// not surface as a current lookup result).
    pub fn get_active_by_path_key(
        &self,
        path_key: &str,
    ) -> Result<Option<Resource>, ResourceError> {
        self.connection
            .query_row(
                &format!("{SELECT_RESOURCE_SQL} WHERE r.path_key = ?1 AND r.state = 'ACTIVE'"),
                params![path_key],
                raw_resource_from_row,
            )
            .optional()?
            .map(decode_resource)
            .transpose()
    }

    /// List every Resource row, ordered by `path_key` for deterministic
    /// results.
    pub fn list(&self) -> Result<Vec<Resource>, ResourceError> {
        let mut statement = self
            .connection
            .prepare(&format!("{SELECT_RESOURCE_SQL} ORDER BY r.path_key"))?;
        let rows = statement.query_map([], raw_resource_from_row)?;
        rows.map(|raw| decode_resource(raw?)).collect()
    }

    /// List the current (ACTIVE) Resource inventory, ordered by `path_key`.
    pub fn list_active(&self) -> Result<Vec<Resource>, ResourceError> {
        let mut statement = self.connection.prepare(&format!(
            "{SELECT_RESOURCE_SQL} WHERE r.state = 'ACTIVE' ORDER BY r.path_key"
        ))?;
        let rows = statement.query_map([], raw_resource_from_row)?;
        rows.map(|raw| decode_resource(raw?)).collect()
    }

    /// Overwrite every mutable column of the row identified by
    /// `resource.id`. The stable id is the key and is never rewritten.
    /// Returns `false` if no row carries that id.
    pub fn update_resource(&self, resource: &Resource) -> Result<bool, ResourceError> {
        let container_local_id = resource
            .container_resource_id
            .map(|container_id| {
                self.local_id(container_id)?
                    .ok_or(ResourceError::UnknownContainer { container_id })
            })
            .transpose()?;

        let changed = self.connection.execute(
            "UPDATE resource SET \
             path_rel = ?2, path_key = ?3, kind = ?4, role = ?5, language = ?6, \
             size = ?7, mtime_ns = ?8, fingerprint = ?9, content_hash = ?10, \
             state = ?11, resource_revision = ?12, generated_kind = ?13, \
             container_resource_id = ?14 \
             WHERE uid = ?1",
            params![
                resource.id.to_bytes().to_vec(),
                resource.path_rel,
                resource.path_key,
                resource.kind.as_str(),
                resource.role.as_str(),
                resource.language.map(ResourceLanguage::as_str),
                resource.size_bytes,
                resource.mtime_ns,
                resource.fingerprint,
                resource.content_hash,
                resource.state.as_str(),
                resource.resource_revision,
                resource.generated_kind,
                container_local_id,
            ],
        )?;
        Ok(changed > 0)
    }

    /// Refresh only the cheap filesystem metadata of an existing Resource,
    /// leaving `fingerprint`/`resource_revision` untouched (#16 task 3: an
    /// mtime-only change is never a semantic change, but recording it keeps
    /// the metadata fast path usable next time).
    pub fn refresh_metadata(
        &self,
        id: ResourceId,
        size_bytes: i64,
        mtime_ns: i64,
    ) -> Result<bool, ResourceError> {
        let changed = self.connection.execute(
            "UPDATE resource SET size = ?2, mtime_ns = ?3 WHERE uid = ?1",
            params![id.to_bytes().to_vec(), size_bytes, mtime_ns],
        )?;
        Ok(changed > 0)
    }

    /// Transition an ACTIVE Resource to DELETED, preserving its stable id
    /// and its user-facing historical `path_rel` while moving its internal
    /// `path_key` into [`TOMBSTONE_PATH_KEY_PREFIX`] so the freed path stays
    /// available to a future Resource.
    ///
    /// Idempotent: an already-DELETED row matches nothing and is returned as
    /// `false`, so repeated deletes never bump the revision again.
    pub fn mark_deleted(
        &self,
        id: ResourceId,
        resource_revision: &str,
    ) -> Result<bool, ResourceError> {
        let changed = self.connection.execute(
            "UPDATE resource SET \
             state = 'DELETED', \
             resource_revision = ?2, \
             path_key = ?3 || hex(uid) || '/' || path_key \
             WHERE uid = ?1 AND state = 'ACTIVE'",
            params![
                id.to_bytes().to_vec(),
                resource_revision,
                TOMBSTONE_PATH_KEY_PREFIX
            ],
        )?;
        Ok(changed > 0)
    }

    /// Begin a transaction on this store's connection. Every subsequent
    /// call on this same `ResourceStore` runs inside it until the returned
    /// transaction is committed or dropped (rollback).
    pub fn transaction(&self) -> Result<rusqlite::Transaction<'_>, ResourceError> {
        Ok(self.connection.unchecked_transaction()?)
    }

    /// This store's `index.db` connection, so a caller that must write
    /// *other* `index.db` tables in the same transaction as a Resource
    /// apply -- #16 task 4's baseline publication -- can do so on one
    /// connection. Crate-internal: the connection is not public API.
    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }

    fn local_id(&self, id: ResourceId) -> Result<Option<i64>, ResourceError> {
        self.connection
            .query_row(
                "SELECT id FROM resource WHERE uid = ?1",
                params![id.to_bytes().to_vec()],
                |row| row.get(0),
            )
            .optional()
            .map_err(ResourceError::from)
    }
}

/// Raw column values as stored, before closed-vocabulary decoding.
pub(crate) struct RawResourceRow {
    uid: Vec<u8>,
    path_rel: String,
    path_key: String,
    kind: String,
    role: String,
    language: Option<String>,
    size_bytes: i64,
    mtime_ns: i64,
    fingerprint: String,
    content_hash: Option<String>,
    state: String,
    resource_revision: String,
    generated_kind: Option<String>,
    container_uid: Option<Vec<u8>>,
}

pub(crate) fn raw_resource_from_row(row: &Row<'_>) -> rusqlite::Result<RawResourceRow> {
    Ok(RawResourceRow {
        uid: row.get(0)?,
        path_rel: row.get(1)?,
        path_key: row.get(2)?,
        kind: row.get(3)?,
        role: row.get(4)?,
        language: row.get(5)?,
        size_bytes: row.get(6)?,
        mtime_ns: row.get(7)?,
        fingerprint: row.get(8)?,
        content_hash: row.get(9)?,
        state: row.get(10)?,
        resource_revision: row.get(11)?,
        generated_kind: row.get(12)?,
        container_uid: row.get(13)?,
    })
}

pub(crate) fn decode_resource(raw: RawResourceRow) -> Result<Resource, ResourceError> {
    Ok(Resource {
        id: resource_id_from_blob(raw.uid),
        path_rel: raw.path_rel,
        path_key: raw.path_key,
        kind: ResourceKind::parse(&raw.kind)?,
        role: ResourceRole::parse(&raw.role)?,
        language: raw
            .language
            .as_deref()
            .map(ResourceLanguage::parse)
            .transpose()?,
        size_bytes: raw.size_bytes,
        mtime_ns: raw.mtime_ns,
        fingerprint: raw.fingerprint,
        content_hash: raw.content_hash,
        state: ResourceState::parse(&raw.state)?,
        resource_revision: raw.resource_revision,
        generated_kind: raw.generated_kind,
        container_resource_id: raw.container_uid.map(resource_id_from_blob),
    })
}

fn resource_id_from_blob(bytes: Vec<u8>) -> ResourceId {
    let array: [u8; 16] = bytes
        .as_slice()
        .try_into()
        .expect("resource.uid column must always hold a 16-byte value written by this module");
    ResourceId::from_bytes(array)
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs, process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "brainprint-resource-{label}-{}-{sequence}",
                process::id()
            ));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self(path)
        }

        fn db_path(&self) -> std::path::PathBuf {
            self.0.join("data").join("index.db")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sample(path_rel: &str) -> Resource {
        Resource {
            id: ResourceId::generate(),
            path_rel: path_rel.to_owned(),
            path_key: path_rel.to_owned(),
            kind: ResourceKind::File,
            role: ResourceRole::Source,
            language: Some(ResourceLanguage::Rust),
            size_bytes: 128,
            mtime_ns: 1_700_000_000_000,
            fingerprint: "fp-1".to_owned(),
            content_hash: Some("hash-1".to_owned()),
            state: ResourceState::Active,
            resource_revision: "rev-1".to_owned(),
            generated_kind: None,
            container_resource_id: None,
        }
    }

    #[test]
    fn resource_typed_value_round_trips_through_the_db() {
        let dir = TestDir::create("round-trip");
        let store = ResourceStore::open(&dir.db_path()).expect("store should open");
        let resource = sample("src/lib.rs");

        store
            .insert_resource(&resource)
            .expect("insert should succeed");

        let reloaded = store
            .get_by_id(resource.id)
            .expect("lookup should succeed")
            .expect("resource should exist");
        assert_eq!(reloaded, resource);
    }

    #[test]
    fn resource_id_uniqueness_is_enforced() {
        let dir = TestDir::create("id-uniqueness");
        let store = ResourceStore::open(&dir.db_path()).expect("store should open");
        let mut first = sample("a.rs");
        let mut second = sample("b.rs");
        second.id = first.id;
        first.path_key = "a.rs".to_owned();
        second.path_key = "b.rs".to_owned();

        store.insert_resource(&first).expect("first insert ok");
        let error = store
            .insert_resource(&second)
            .expect_err("duplicate stable id must be rejected");
        assert!(matches!(
            error,
            ResourceError::Sqlite(rusqlite::Error::SqliteFailure(inner, _))
            if inner.code == rusqlite::ErrorCode::ConstraintViolation
        ));
    }

    #[test]
    fn path_key_uniqueness_is_enforced() {
        let dir = TestDir::create("path-key-uniqueness");
        let store = ResourceStore::open(&dir.db_path()).expect("store should open");
        let first = sample("dup.rs");
        let mut second = sample("dup.rs");
        second.id = ResourceId::generate();

        store.insert_resource(&first).expect("first insert ok");
        let error = store
            .insert_resource(&second)
            .expect_err("duplicate path_key must be rejected");
        assert!(matches!(
            error,
            ResourceError::Sqlite(rusqlite::Error::SqliteFailure(inner, _))
            if inner.code == rusqlite::ErrorCode::ConstraintViolation
        ));
    }

    #[test]
    fn stable_id_lookup_returns_the_matching_resource() {
        let dir = TestDir::create("id-lookup");
        let store = ResourceStore::open(&dir.db_path()).expect("store should open");
        let resource = sample("src/main.rs");
        store.insert_resource(&resource).expect("insert ok");

        let found = store
            .get_by_id(resource.id)
            .expect("lookup should succeed")
            .expect("resource should be found");
        assert_eq!(found.id, resource.id);

        assert!(
            store
                .get_by_id(ResourceId::generate())
                .expect("lookup should succeed")
                .is_none()
        );
    }

    #[test]
    fn path_key_lookup_returns_the_matching_resource() {
        let dir = TestDir::create("path-key-lookup");
        let store = ResourceStore::open(&dir.db_path()).expect("store should open");
        let resource = sample("src/found.rs");
        store.insert_resource(&resource).expect("insert ok");

        let found = store
            .get_by_path_key("src/found.rs")
            .expect("lookup should succeed")
            .expect("resource should be found");
        assert_eq!(found.id, resource.id);

        assert!(
            store
                .get_by_path_key("src/missing.rs")
                .expect("lookup should succeed")
                .is_none()
        );
    }

    #[test]
    fn list_round_trips_every_inserted_resource() {
        let dir = TestDir::create("list");
        let store = ResourceStore::open(&dir.db_path()).expect("store should open");
        let one = sample("a.rs");
        let two = sample("b.rs");
        store.insert_resource(&one).expect("insert one ok");
        store.insert_resource(&two).expect("insert two ok");

        let all = store.list().expect("list should succeed");
        assert_eq!(all, vec![one, two], "list must be ordered by path_key");
    }

    #[test]
    fn container_resource_id_round_trips_by_stable_id() {
        let dir = TestDir::create("container");
        let store = ResourceStore::open(&dir.db_path()).expect("store should open");
        let container = sample("gen/bundle.js");
        store
            .insert_resource(&container)
            .expect("container insert ok");

        let mut child = sample("src/app.svelte");
        child.container_resource_id = Some(container.id);
        store.insert_resource(&child).expect("child insert ok");

        let reloaded = store
            .get_by_id(child.id)
            .expect("lookup should succeed")
            .expect("child should exist");
        assert_eq!(reloaded.container_resource_id, Some(container.id));
    }

    #[test]
    fn unknown_container_is_rejected() {
        let dir = TestDir::create("unknown-container");
        let store = ResourceStore::open(&dir.db_path()).expect("store should open");
        let mut resource = sample("orphan.rs");
        let missing_container = ResourceId::generate();
        resource.container_resource_id = Some(missing_container);

        let error = store
            .insert_resource(&resource)
            .expect_err("nonexistent container must be rejected");
        assert!(matches!(
            error,
            ResourceError::UnknownContainer { container_id } if container_id == missing_container
        ));
    }

    #[test]
    fn invalid_kind_value_is_explicitly_rejected() {
        let dir = TestDir::create("invalid-kind");
        let store = ResourceStore::open(&dir.db_path()).expect("store should open");
        insert_raw_with_column(&store, "kind", "BOGUS");

        let error = store
            .get_by_path_key("raw.rs")
            .expect_err("unrecognized kind must not silently decode");
        assert!(matches!(
            error,
            ResourceError::UnknownResourceKind { raw } if raw == "BOGUS"
        ));
    }

    #[test]
    fn invalid_role_value_is_explicitly_rejected() {
        let dir = TestDir::create("invalid-role");
        let store = ResourceStore::open(&dir.db_path()).expect("store should open");
        insert_raw_with_column(&store, "role", "BOGUS");

        let error = store
            .get_by_path_key("raw.rs")
            .expect_err("unrecognized role must not silently decode");
        assert!(matches!(
            error,
            ResourceError::UnknownResourceRole { raw } if raw == "BOGUS"
        ));
    }

    #[test]
    fn invalid_language_value_is_explicitly_rejected() {
        let dir = TestDir::create("invalid-language");
        let store = ResourceStore::open(&dir.db_path()).expect("store should open");
        insert_raw_with_column(&store, "language", "KLINGON");

        let error = store
            .get_by_path_key("raw.rs")
            .expect_err("unrecognized language must not silently decode");
        assert!(matches!(
            error,
            ResourceError::UnknownResourceLanguage { raw } if raw == "KLINGON"
        ));
    }

    #[test]
    fn invalid_state_value_is_explicitly_rejected() {
        let dir = TestDir::create("invalid-state");
        let store = ResourceStore::open(&dir.db_path()).expect("store should open");
        insert_raw_with_column(&store, "state", "BOGUS");

        let error = store
            .get_by_path_key("raw.rs")
            .expect_err("unrecognized state must not silently decode");
        assert!(matches!(
            error,
            ResourceError::UnknownResourceState { raw } if raw == "BOGUS"
        ));
    }

    /// Inserts a row bypassing the typed API, with `column` overridden to
    /// an out-of-vocabulary raw value, to prove reads reject it explicitly
    /// instead of silently reinterpreting it.
    fn insert_raw_with_column(store: &ResourceStore, column: &str, value: &str) {
        let base = sample("raw.rs");
        store
            .connection
            .execute(
                "INSERT INTO resource \
                 (uid, path_rel, path_key, kind, role, language, size, mtime_ns, \
                  fingerprint, content_hash, state, resource_revision) \
                 VALUES (?1, ?2, ?2, 'FILE', 'SOURCE', 'RUST', 0, 0, 'fp', NULL, 'ACTIVE', 'rev-1')",
                params![base.id.to_bytes().to_vec(), base.path_rel],
            )
            .expect("raw insert should succeed");
        store
            .connection
            .execute(
                &format!("UPDATE resource SET {column} = ?1 WHERE path_key = 'raw.rs'"),
                params![value],
            )
            .expect("raw column override should succeed");
    }

    #[test]
    fn no_source_body_column_exists_on_resource() {
        let dir = TestDir::create("no-source-body");
        let store = ResourceStore::open(&dir.db_path()).expect("store should open");

        let mut statement = store
            .connection
            .prepare("PRAGMA table_info(resource)")
            .expect("table_info should be queryable");
        let columns: Vec<String> = statement
            .query_map([], |row| row.get::<_, String>(1))
            .expect("column listing should succeed")
            .collect::<Result<_, _>>()
            .expect("column names should decode");

        const DISALLOWED: &[&str] = &[
            "content",
            "source_body",
            "raw_content",
            "file_content",
            "body_text",
            "source_text",
        ];
        for column in &columns {
            assert!(
                !DISALLOWED.contains(&column.to_lowercase().as_str()),
                "resource.{column} looks like a raw source body column"
            );
        }
    }
}
