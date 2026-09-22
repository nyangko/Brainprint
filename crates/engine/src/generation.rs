//! `index.db` revision/generation foundation: the BUILDING→STABLE/ABORTED
//! publication contract (#15 task 8 / #4 task 1 §6, #13 task 7 §2, §4-5).
//!
//! Scope: this module owns only the `generation`/`workspace_clock` rows
//! already created by #15 task 5's schema -- it defines no new tables and
//! redesigns none of the confirmed task 5 columns. It has no knowledge of:
//! - watcher/change detection that would actually advance
//!   `current_workspace_revision` (#4 task 2, out of scope -- I2+)
//! - Resource discovery, structural/semantic analysis that would produce
//!   what a generation publishes (#15 task 8 explicitly excludes this)
//! - `component_state` per-component freshness (later task)
//!
//! Core contract (#13 task 7 §4-5): a `generation` row is `BUILDING`,
//! `STABLE`, or `ABORTED`. Only `workspace_clock.stable_generation_id` is
//! Agent-facing "current" -- a `BUILDING` row is never read as current
//! regardless of how long it has existed, including after a process
//! restart (#15 task 8 DoD). Publishing to `STABLE` re-checks the
//! generation's `basis_workspace_revision` against the *current*
//! `workspace_clock.current_workspace_revision` and, in the same
//! transaction, flips `generation.state` to `STABLE` and swaps
//! `workspace_clock.stable_generation_id` -- both happen atomically or
//! neither does. A stale basis is never published STABLE; the generation
//! is instead transitioned to `ABORTED` (#13 task 7 §5: "input mismatch:
//! generation ABORTED[,] stable pointer unchanged") and the existing
//! stable generation, if any, is left exactly as it was.

use std::{error::Error, fmt, path::Path};

use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    db::{self, DbOpenError},
    schema,
};

/// A `generation` row's publication state (#13 task 7 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationState {
    Building,
    Stable,
    Aborted,
}

impl GenerationState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Building => "BUILDING",
            Self::Stable => "STABLE",
            Self::Aborted => "ABORTED",
        }
    }

    fn parse(raw: &str) -> Result<Self, GenerationError> {
        match raw {
            "BUILDING" => Ok(Self::Building),
            "STABLE" => Ok(Self::Stable),
            "ABORTED" => Ok(Self::Aborted),
            other => Err(GenerationError::UnknownGenerationState {
                raw: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for GenerationState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One `generation` row: a publication unit (#4 task 1 §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationRecord {
    pub id: i64,
    pub generation_no: i64,
    pub basis_workspace_revision: String,
    pub state: GenerationState,
    pub created_at: String,
    pub published_at: Option<String>,
    pub aborted_reason: Option<String>,
}

/// Failure creating, publishing, aborting, or reading a generation, or the
/// Workspace revision clock.
#[derive(Debug)]
pub enum GenerationError {
    Open(DbOpenError),
    Sqlite(rusqlite::Error),
    /// `workspace_clock` has no row yet; callers must bootstrap it first.
    ClockNotBootstrapped,
    UnknownGeneration {
        generation_id: i64,
    },
    /// A publish/abort was requested for a generation that is not (or is
    /// no longer) `BUILDING` -- a `STABLE` or `ABORTED` generation is a
    /// completed result, never re-published or re-aborted.
    NotBuilding {
        generation_id: i64,
        state: GenerationState,
    },
    /// The generation's `basis_workspace_revision` no longer matches the
    /// current `workspace_clock.current_workspace_revision`; the
    /// generation was transitioned to `ABORTED` rather than published.
    ObsoleteBasisRevision {
        generation_id: i64,
        basis: String,
        current: String,
    },
    /// `generation.state` held a value this module never writes.
    UnknownGenerationState {
        raw: String,
    },
    /// `workspace_clock.stable_generation_id` pointed at a generation that
    /// is not `STABLE` -- the one invariant this module must never violate
    /// (#15 task 8: BUILDING is never exposed as current).
    StableInvariantViolated {
        generation_id: i64,
        state: GenerationState,
    },
}

impl fmt::Display for GenerationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(source) => write!(formatter, "failed to open index.db: {source}"),
            Self::Sqlite(source) => write!(formatter, "generation store sqlite error: {source}"),
            Self::ClockNotBootstrapped => {
                formatter.write_str("workspace_clock has not been bootstrapped")
            }
            Self::UnknownGeneration { generation_id } => {
                write!(formatter, "no generation row for id {generation_id}")
            }
            Self::NotBuilding {
                generation_id,
                state,
            } => write!(
                formatter,
                "generation {generation_id} is {state}, not BUILDING"
            ),
            Self::ObsoleteBasisRevision {
                generation_id,
                basis,
                current,
            } => write!(
                formatter,
                "generation {generation_id}'s basis workspace revision {basis} no longer \
                 matches current workspace revision {current}; aborted instead of published"
            ),
            Self::UnknownGenerationState { raw } => {
                write!(formatter, "unknown generation state {raw:?}")
            }
            Self::StableInvariantViolated {
                generation_id,
                state,
            } => write!(
                formatter,
                "workspace_clock's stable_generation_id points at generation \
                 {generation_id}, which is {state}, not STABLE"
            ),
        }
    }
}

impl Error for GenerationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Open(source) => Some(source),
            Self::Sqlite(source) => Some(source),
            Self::ClockNotBootstrapped
            | Self::UnknownGeneration { .. }
            | Self::NotBuilding { .. }
            | Self::ObsoleteBasisRevision { .. }
            | Self::UnknownGenerationState { .. }
            | Self::StableInvariantViolated { .. } => None,
        }
    }
}

impl From<DbOpenError> for GenerationError {
    fn from(source: DbOpenError) -> Self {
        Self::Open(source)
    }
}

impl From<rusqlite::Error> for GenerationError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

/// `watcher_continuity_state` is a `workspace_clock` column owned by the
/// watcher task (#4 task 2, out of scope here). This module only needs to
/// seed a non-NULL placeholder when bootstrapping the clock row.
const WATCHER_CONTINUITY_UNINITIALIZED: &str = "UNINITIALIZED";

/// Handle to one Workspace's `index.db` revision/generation tables.
pub struct GenerationStore {
    connection: Connection,
}

impl GenerationStore {
    /// Open (creating/migrating if needed) the `index.db` at `path`.
    pub fn open(path: &Path) -> Result<Self, GenerationError> {
        let opened = schema::index::open(path)?;
        Ok(Self::from_connection(opened.connection))
    }

    /// Wrap an already-opened `index.db` connection (#15 task 11: lets a
    /// caller that already opened/verified `index.db` -- e.g.
    /// `engine::init`'s reopen path -- reuse that same connection for
    /// orphan-generation reconciliation instead of opening a second one).
    #[must_use]
    pub fn from_connection(connection: Connection) -> Self {
        Self { connection }
    }

    /// Create `workspace_clock`'s single row if it does not already exist.
    /// Idempotent: an existing row is left untouched.
    pub fn bootstrap_clock(&self, initial_workspace_revision: &str) -> Result<(), GenerationError> {
        bootstrap_clock(&self.connection, initial_workspace_revision)
    }

    /// Read the current Workspace input revision, or `None` if the clock
    /// has not been bootstrapped yet.
    pub fn current_workspace_revision(&self) -> Result<Option<String>, GenerationError> {
        current_workspace_revision(&self.connection)
    }

    /// Explicitly set the current Workspace input revision. This is the
    /// bare write primitive only -- no change detection, journaling, or
    /// component recomputation (that is the watcher's job, out of scope).
    pub fn set_current_workspace_revision(&self, revision: &str) -> Result<(), GenerationError> {
        let changed = self.connection.execute(
            "UPDATE workspace_clock SET current_workspace_revision = ?1 WHERE id = 0",
            params![revision],
        )?;
        if changed == 0 {
            return Err(GenerationError::ClockNotBootstrapped);
        }
        Ok(())
    }

    /// Start a new `BUILDING` generation against `basis_workspace_revision`.
    pub fn begin_generation(
        &self,
        basis_workspace_revision: &str,
    ) -> Result<GenerationRecord, GenerationError> {
        begin_generation(&self.connection, basis_workspace_revision)
    }

    /// Look up a generation by its row id.
    pub fn get_generation(
        &self,
        generation_id: i64,
    ) -> Result<Option<GenerationRecord>, GenerationError> {
        query_generation(&self.connection, generation_id)
    }

    /// Explicitly abort a `BUILDING` generation. Rejects anything not
    /// currently `BUILDING` (#15 task 8: a completed result is never
    /// silently re-taken over).
    pub fn abort_generation(
        &self,
        generation_id: i64,
        reason: &str,
    ) -> Result<GenerationRecord, GenerationError> {
        abort_generation(&self.connection, generation_id, reason)
    }

    /// Abort every `BUILDING` generation left over from a previous process
    /// (#15 task 11 / #4 task 5 §8: a restart discards process-local build
    /// state, and a `BUILDING` row is never promoted to `STABLE` just
    /// because nothing is actively building it anymore -- it is explicitly
    /// marked `ABORTED`/recovery-required instead). Safe to call on every
    /// reopen: an `index.db` with no orphaned `BUILDING` row returns an
    /// empty list. Never touches `workspace_clock.stable_generation_id` --
    /// the existing stable generation, if any, is left exactly as it was.
    pub fn reconcile_orphan_generations(&self) -> Result<Vec<GenerationRecord>, GenerationError> {
        let building_ids: Vec<i64> = {
            let mut statement = self
                .connection
                .prepare("SELECT id FROM generation WHERE state = ?1")?;
            statement
                .query_map(params![GenerationState::Building.as_str()], |row| {
                    row.get(0)
                })?
                .collect::<Result<_, _>>()?
        };

        building_ids
            .into_iter()
            .map(|generation_id| {
                self.abort_generation(
                    generation_id,
                    "orphaned: no active build session after reopen",
                )
            })
            .collect()
    }

    /// Publish a `BUILDING` generation to `STABLE`, atomically swapping
    /// `workspace_clock.stable_generation_id` in the same transaction
    /// (#13 task 7 §5).
    ///
    /// Re-checks `basis_workspace_revision` against the current
    /// `workspace_clock.current_workspace_revision` first: a mismatch
    /// means the build's input is obsolete, so this transitions the
    /// generation to `ABORTED` instead (leaving the existing stable
    /// pointer untouched) and returns
    /// [`GenerationError::ObsoleteBasisRevision`].
    pub fn publish_stable(
        &mut self,
        generation_id: i64,
    ) -> Result<GenerationRecord, GenerationError> {
        let tx = self.connection.transaction()?;

        let generation = match check_publishable(&tx, generation_id) {
            Ok(generation) => generation,
            Err(GenerationError::ObsoleteBasisRevision {
                generation_id,
                basis,
                current,
            }) => {
                abort_obsolete(&tx, generation_id, &basis, &current)?;
                tx.commit()?;
                return Err(GenerationError::ObsoleteBasisRevision {
                    generation_id,
                    basis,
                    current,
                });
            }
            Err(other) => return Err(other),
        };

        let published = finish_publish_stable(&tx, &generation)?;
        tx.commit()?;
        Ok(published)
    }

    /// Read the current stable generation, or `None` if none has been
    /// published yet. Never returns a `BUILDING` (or `ABORTED`) row: if
    /// `workspace_clock.stable_generation_id` somehow pointed at one, that
    /// is an invariant violation and this returns an error rather than
    /// exposing it as current (#15 task 8 DoD).
    pub fn current_stable(&self) -> Result<Option<GenerationRecord>, GenerationError> {
        current_stable(&self.connection)
    }
}

// The `&Connection` primitives below are what [`GenerationStore`]'s methods
// are built from. They exist so a caller that must commit *other* rows in
// the same transaction as a publication -- #16 task 4's Resource baseline
// scan -- can reuse this exact logic inside its own transaction instead of
// re-implementing the publication contract (#16 task 4: "기존
// `GenerationStore::publish_stable` 로직을 복제하지 말고").

pub(crate) fn bootstrap_clock(
    connection: &Connection,
    initial_workspace_revision: &str,
) -> Result<(), GenerationError> {
    connection.execute(
        "INSERT OR IGNORE INTO workspace_clock \
         (id, current_workspace_revision, stable_generation_id, \
          last_change_seq, last_reconcile_seq, last_full_reconcile_at, \
          watcher_continuity_state) \
         VALUES (0, ?1, NULL, 0, 0, NULL, ?2)",
        params![initial_workspace_revision, WATCHER_CONTINUITY_UNINITIALIZED],
    )?;
    Ok(())
}

pub(crate) fn current_workspace_revision(
    connection: &Connection,
) -> Result<Option<String>, GenerationError> {
    connection
        .query_row(
            "SELECT current_workspace_revision FROM workspace_clock WHERE id = 0",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(GenerationError::from)
}

/// `workspace_clock.last_change_seq`: the monotonic input-change sequence
/// (#4 task 1 §4, #13 task 7 §2). Reconcile is what advances it -- a raw
/// watcher event never does.
pub(crate) fn change_seq(connection: &Connection) -> Result<i64, GenerationError> {
    connection
        .query_row(
            "SELECT last_change_seq FROM workspace_clock WHERE id = 0",
            [],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(GenerationError::ClockNotBootstrapped)
}

/// Advance the input-change sequence and the Workspace revision together,
/// in the caller's transaction. Both are one fact -- "a confirmed input
/// change happened" -- so they are never written apart.
pub(crate) fn advance_change_seq(
    connection: &Connection,
    change_seq: i64,
    workspace_revision: &str,
) -> Result<(), GenerationError> {
    let changed = connection.execute(
        "UPDATE workspace_clock SET last_change_seq = ?1, current_workspace_revision = ?2 \
         WHERE id = 0",
        params![change_seq, workspace_revision],
    )?;
    if changed == 0 {
        return Err(GenerationError::ClockNotBootstrapped);
    }
    Ok(())
}

/// Record how far a completed reconcile accounted for the change journal,
/// and when it ran. Bookkeeping only: it says nothing about whether the
/// Resource inventory changed, and it never touches the watcher's own
/// `watcher_continuity_state`, which the watcher contract owns.
pub(crate) fn record_reconcile(
    connection: &Connection,
    last_reconcile_seq: i64,
) -> Result<(), GenerationError> {
    let changed = connection.execute(
        "UPDATE workspace_clock SET last_reconcile_seq = ?1, last_full_reconcile_at = ?2 \
         WHERE id = 0",
        params![last_reconcile_seq, db::now_millis_text()],
    )?;
    if changed == 0 {
        return Err(GenerationError::ClockNotBootstrapped);
    }
    Ok(())
}

/// `workspace_clock.last_reconcile_seq`.
pub(crate) fn last_reconcile_seq(connection: &Connection) -> Result<i64, GenerationError> {
    connection
        .query_row(
            "SELECT last_reconcile_seq FROM workspace_clock WHERE id = 0",
            [],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(GenerationError::ClockNotBootstrapped)
}

pub(crate) fn begin_generation(
    connection: &Connection,
    basis_workspace_revision: &str,
) -> Result<GenerationRecord, GenerationError> {
    let now = db::now_millis_text();
    // The next number and the row that takes it are one statement, not
    // a read followed by a write. Two owners of one AnalysisContext
    // refreshing concurrently on separate connections would otherwise
    // both read the same maximum and the loser would hit the unique
    // index on `generation_no` (#19 task 14).
    connection.execute(
        "INSERT INTO generation \
         (generation_no, basis_workspace_revision, state, created_at) \
         SELECT COALESCE(MAX(generation_no), 0) + 1, ?1, ?2, ?3 FROM generation",
        params![
            basis_workspace_revision,
            GenerationState::Building.as_str(),
            now
        ],
    )?;
    let id = connection.last_insert_rowid();
    let generation_no: i64 = connection.query_row(
        "SELECT generation_no FROM generation WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )?;

    Ok(GenerationRecord {
        id,
        generation_no,
        basis_workspace_revision: basis_workspace_revision.to_owned(),
        state: GenerationState::Building,
        created_at: now,
        published_at: None,
        aborted_reason: None,
    })
}

pub(crate) fn abort_generation(
    connection: &Connection,
    generation_id: i64,
    reason: &str,
) -> Result<GenerationRecord, GenerationError> {
    let generation = query_generation(connection, generation_id)?
        .ok_or(GenerationError::UnknownGeneration { generation_id })?;
    if generation.state != GenerationState::Building {
        return Err(GenerationError::NotBuilding {
            generation_id,
            state: generation.state,
        });
    }

    connection.execute(
        "UPDATE generation SET state = ?1, aborted_reason = ?2 WHERE id = ?3",
        params![GenerationState::Aborted.as_str(), reason, generation_id],
    )?;

    Ok(GenerationRecord {
        state: GenerationState::Aborted,
        aborted_reason: Some(reason.to_owned()),
        ..generation
    })
}

/// Steps 1-2 of the publication contract: the generation is still
/// `BUILDING` and its `basis_workspace_revision` still matches the clock.
/// Writes nothing -- an obsolete basis is reported as
/// [`GenerationError::ObsoleteBasisRevision`] so the caller decides whether
/// its own transaction can carry the abort or has to roll back first.
pub(crate) fn check_publishable(
    connection: &Connection,
    generation_id: i64,
) -> Result<GenerationRecord, GenerationError> {
    let generation = query_generation(connection, generation_id)?
        .ok_or(GenerationError::UnknownGeneration { generation_id })?;
    if generation.state != GenerationState::Building {
        return Err(GenerationError::NotBuilding {
            generation_id,
            state: generation.state,
        });
    }

    let Some(current) = current_workspace_revision(connection)? else {
        return Err(GenerationError::ClockNotBootstrapped);
    };
    if current != generation.basis_workspace_revision {
        return Err(GenerationError::ObsoleteBasisRevision {
            generation_id,
            basis: generation.basis_workspace_revision,
            current,
        });
    }

    Ok(generation)
}

/// Proof that a generation was found publishable *inside the caller's own
/// open publication transaction* (#16 task 13).
///
/// It exists so that evidence which normally may only be attached to the
/// current STABLE generation -- Occurrences (#16 task 9) -- can be written
/// against the generation that is about to become stable in this very
/// transaction, without opening a general "write evidence to any BUILDING
/// generation" API. Its fields are private to this module, so the only way
/// to hold one is to have called [`grant_publication`], which re-runs
/// [`check_publishable`]: still BUILDING, and its basis still the current
/// Workspace revision.
///
/// What the token cannot prove on its own is the last part of the
/// contract: the holder must transition the generation to STABLE
/// ([`finish_publish_stable`]) before committing, or roll the whole
/// transaction back. A grant that is neither published nor rolled back
/// would leave evidence on a BUILDING generation, which is what the
/// STABLE-only rule exists to prevent.
///
/// The type is public because it appears in the signatures a publication
/// path calls (#17 task 3), but it stays sealed: its fields are private
/// and [`grant_publication`] is the only way to obtain one, so nothing
/// outside this crate can manufacture the permission.
pub struct PublicationGrant {
    generation_id: i64,
    basis_workspace_revision: String,
}

impl PublicationGrant {
    pub(crate) const fn generation_id(&self) -> i64 {
        self.generation_id
    }

    /// The Workspace revision this publication is establishing.
    pub(crate) fn basis_workspace_revision(&self) -> &str {
        &self.basis_workspace_revision
    }
}

/// [`check_publishable`], returning the grant token alongside the record.
///
/// `connection` must be the caller's open publication transaction: the
/// checks are only worth anything if nothing can move between them and the
/// writes they authorize.
pub(crate) fn grant_publication(
    connection: &Connection,
    generation_id: i64,
) -> Result<(GenerationRecord, PublicationGrant), GenerationError> {
    let generation = check_publishable(connection, generation_id)?;
    let grant = PublicationGrant {
        generation_id: generation.id,
        basis_workspace_revision: generation.basis_workspace_revision.clone(),
    };
    Ok((generation, grant))
}

/// The obsolete-basis abort, worded identically wherever it is written.
pub(crate) fn abort_obsolete(
    connection: &Connection,
    generation_id: i64,
    basis: &str,
    current: &str,
) -> Result<(), GenerationError> {
    let reason = format!(
        "basis workspace revision {basis} no longer matches current workspace revision {current}"
    );
    connection.execute(
        "UPDATE generation SET state = ?1, aborted_reason = ?2 WHERE id = ?3",
        params![GenerationState::Aborted.as_str(), reason, generation_id],
    )?;
    Ok(())
}

/// The final two writes of the publication contract: `generation` →
/// `STABLE` and the `stable_generation_id` swap. Must run in the same
/// transaction as everything else the generation publishes, after
/// [`check_publishable`].
pub(crate) fn finish_publish_stable(
    connection: &Connection,
    generation: &GenerationRecord,
) -> Result<GenerationRecord, GenerationError> {
    let now = db::now_millis_text();
    connection.execute(
        "UPDATE generation SET state = ?1, published_at = ?2 WHERE id = ?3",
        params![GenerationState::Stable.as_str(), now, generation.id],
    )?;
    connection.execute(
        "UPDATE workspace_clock SET stable_generation_id = ?1 WHERE id = 0",
        params![generation.id],
    )?;

    Ok(GenerationRecord {
        state: GenerationState::Stable,
        published_at: Some(now),
        ..generation.clone()
    })
}

pub(crate) fn current_stable(
    connection: &Connection,
) -> Result<Option<GenerationRecord>, GenerationError> {
    let stable_id: Option<i64> = connection
        .query_row(
            "SELECT stable_generation_id FROM workspace_clock WHERE id = 0",
            [],
            |row| row.get(0),
        )
        .optional()?
        .flatten();

    let Some(stable_id) = stable_id else {
        return Ok(None);
    };

    let generation =
        query_generation(connection, stable_id)?.ok_or(GenerationError::UnknownGeneration {
            generation_id: stable_id,
        })?;
    if generation.state != GenerationState::Stable {
        return Err(GenerationError::StableInvariantViolated {
            generation_id: stable_id,
            state: generation.state,
        });
    }

    Ok(Some(generation))
}

fn query_generation(
    connection: &Connection,
    generation_id: i64,
) -> Result<Option<GenerationRecord>, GenerationError> {
    connection
        .query_row(
            "SELECT id, generation_no, basis_workspace_revision, state, created_at, \
                    published_at, aborted_reason \
             FROM generation WHERE id = ?1",
            params![generation_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()?
        .map(
            |(
                id,
                generation_no,
                basis_workspace_revision,
                state_raw,
                created_at,
                published_at,
                aborted_reason,
            )| {
                Ok(GenerationRecord {
                    id,
                    generation_no,
                    basis_workspace_revision,
                    state: GenerationState::parse(&state_raw)?,
                    created_at,
                    published_at,
                    aborted_reason,
                })
            },
        )
        .transpose()
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
                "brainprint-generation-{label}-{}-{sequence}",
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

    #[test]
    fn begin_generation_starts_in_building_state() {
        let dir = TestDir::create("begin");
        let store = GenerationStore::open(&dir.db_path()).expect("store should open");

        let generation = store
            .begin_generation("rev-1")
            .expect("begin should succeed");

        assert_eq!(generation.state, GenerationState::Building);
        assert_eq!(generation.generation_no, 1);
        assert_eq!(generation.basis_workspace_revision, "rev-1");
        assert!(generation.published_at.is_none());
        assert!(generation.aborted_reason.is_none());
    }

    #[test]
    fn generation_numbers_increase_monotonically() {
        let dir = TestDir::create("monotonic");
        let store = GenerationStore::open(&dir.db_path()).expect("store should open");

        let first = store.begin_generation("rev-1").expect("first begin ok");
        let second = store.begin_generation("rev-2").expect("second begin ok");

        assert_eq!(first.generation_no, 1);
        assert_eq!(second.generation_no, 2);
        assert_ne!(first.id, second.id);
    }

    #[test]
    fn publish_stable_succeeds_when_basis_matches_current_revision() {
        let dir = TestDir::create("publish-success");
        let mut store = GenerationStore::open(&dir.db_path()).expect("store should open");
        store
            .bootstrap_clock("rev-1")
            .expect("bootstrap should succeed");

        let generation = store.begin_generation("rev-1").expect("begin ok");
        let published = store
            .publish_stable(generation.id)
            .expect("publish should succeed");

        assert_eq!(published.state, GenerationState::Stable);
        assert!(published.published_at.is_some());

        let current = store
            .current_stable()
            .expect("current lookup should succeed")
            .expect("a stable generation should now exist");
        assert_eq!(current.id, generation.id);
        assert_eq!(current.state, GenerationState::Stable);
    }

    #[test]
    fn building_to_aborted_transition_succeeds() {
        let dir = TestDir::create("abort");
        let store = GenerationStore::open(&dir.db_path()).expect("store should open");

        let generation = store.begin_generation("rev-1").expect("begin ok");
        let aborted = store
            .abort_generation(generation.id, "manual cancel")
            .expect("abort should succeed");

        assert_eq!(aborted.state, GenerationState::Aborted);
        assert_eq!(aborted.aborted_reason.as_deref(), Some("manual cancel"));

        let reloaded = store
            .get_generation(generation.id)
            .expect("lookup should succeed")
            .expect("generation should still exist");
        assert_eq!(reloaded.state, GenerationState::Aborted);
    }

    #[test]
    fn publish_rejects_obsolete_basis_revision_and_aborts_it() {
        let dir = TestDir::create("obsolete");
        let mut store = GenerationStore::open(&dir.db_path()).expect("store should open");
        store.bootstrap_clock("rev-1").expect("bootstrap ok");

        let generation = store.begin_generation("rev-1").expect("begin ok");
        // The Workspace changed while the generation was building.
        store
            .set_current_workspace_revision("rev-2")
            .expect("revision advance should succeed");

        let error = store
            .publish_stable(generation.id)
            .expect_err("stale basis revision must not publish");
        assert!(matches!(
            error,
            GenerationError::ObsoleteBasisRevision { generation_id, .. } if generation_id == generation.id
        ));

        let reloaded = store
            .get_generation(generation.id)
            .expect("lookup should succeed")
            .expect("generation should still exist");
        assert_eq!(reloaded.state, GenerationState::Aborted);

        assert!(
            store
                .current_stable()
                .expect("current lookup should succeed")
                .is_none(),
            "an obsolete generation must never become current"
        );
    }

    #[test]
    fn existing_stable_generation_survives_a_failed_publish() {
        let dir = TestDir::create("survives-failure");
        let mut store = GenerationStore::open(&dir.db_path()).expect("store should open");
        store.bootstrap_clock("rev-1").expect("bootstrap ok");

        let first = store.begin_generation("rev-1").expect("first begin ok");
        store
            .publish_stable(first.id)
            .expect("first publish should succeed");

        let second = store.begin_generation("rev-1").expect("second begin ok");
        store
            .set_current_workspace_revision("rev-2")
            .expect("revision advance should succeed");
        store
            .publish_stable(second.id)
            .expect_err("second publish must fail on stale basis");

        let current = store
            .current_stable()
            .expect("current lookup should succeed")
            .expect("the original stable generation must remain");
        assert_eq!(current.id, first.id);
    }

    #[test]
    fn publishing_an_already_stable_generation_is_rejected() {
        let dir = TestDir::create("republish");
        let mut store = GenerationStore::open(&dir.db_path()).expect("store should open");
        store.bootstrap_clock("rev-1").expect("bootstrap ok");

        let generation = store.begin_generation("rev-1").expect("begin ok");
        store
            .publish_stable(generation.id)
            .expect("first publish should succeed");

        let error = store
            .publish_stable(generation.id)
            .expect_err("re-publishing an already-stable generation must be rejected");
        assert!(matches!(
            error,
            GenerationError::NotBuilding { generation_id, state: GenerationState::Stable }
            if generation_id == generation.id
        ));
    }

    #[test]
    fn aborting_a_stable_generation_is_rejected() {
        let dir = TestDir::create("abort-stable");
        let mut store = GenerationStore::open(&dir.db_path()).expect("store should open");
        store.bootstrap_clock("rev-1").expect("bootstrap ok");

        let generation = store.begin_generation("rev-1").expect("begin ok");
        store
            .publish_stable(generation.id)
            .expect("publish should succeed");

        let error = store
            .abort_generation(generation.id, "too late")
            .expect_err("a stable generation must never be silently re-taken over");
        assert!(matches!(
            error,
            GenerationError::NotBuilding {
                state: GenerationState::Stable,
                ..
            }
        ));
    }

    #[test]
    fn reopen_never_exposes_a_building_generation_as_current() {
        let dir = TestDir::create("reopen-building");
        {
            let store = GenerationStore::open(&dir.db_path()).expect("store should open");
            store.bootstrap_clock("rev-1").expect("bootstrap ok");
            store.begin_generation("rev-1").expect("begin ok");
            // No publish_stable call: this generation stays BUILDING.
        }

        let reopened = GenerationStore::open(&dir.db_path()).expect("store should reopen");
        let current = reopened
            .current_stable()
            .expect("current lookup should succeed");
        assert!(
            current.is_none(),
            "a BUILDING generation must never be treated as current, even after reopen"
        );
    }

    #[test]
    fn reopen_preserves_the_published_stable_generation() {
        let dir = TestDir::create("reopen-stable");
        let published_id;
        {
            let mut store = GenerationStore::open(&dir.db_path()).expect("store should open");
            store.bootstrap_clock("rev-1").expect("bootstrap ok");
            let generation = store.begin_generation("rev-1").expect("begin ok");
            let published = store
                .publish_stable(generation.id)
                .expect("publish should succeed");
            published_id = published.id;
        }

        let reopened = GenerationStore::open(&dir.db_path()).expect("store should reopen");
        let current = reopened
            .current_stable()
            .expect("current lookup should succeed")
            .expect("the published generation should survive reopen");
        assert_eq!(current.id, published_id);
        assert_eq!(current.state, GenerationState::Stable);
    }

    #[test]
    fn publish_without_bootstrapped_clock_is_rejected() {
        let dir = TestDir::create("no-clock");
        let mut store = GenerationStore::open(&dir.db_path()).expect("store should open");

        let generation = store.begin_generation("rev-1").expect("begin ok");
        let error = store
            .publish_stable(generation.id)
            .expect_err("publishing without a bootstrapped clock must be rejected");
        assert!(matches!(error, GenerationError::ClockNotBootstrapped));
    }

    #[test]
    fn unknown_generation_id_is_rejected() {
        let dir = TestDir::create("unknown");
        let mut store = GenerationStore::open(&dir.db_path()).expect("store should open");
        store.bootstrap_clock("rev-1").expect("bootstrap ok");

        let error = store
            .publish_stable(999)
            .expect_err("an unknown generation id must be rejected");
        assert!(matches!(
            error,
            GenerationError::UnknownGeneration { generation_id: 999 }
        ));

        assert!(
            store
                .get_generation(999)
                .expect("lookup should succeed")
                .is_none()
        );
    }

    #[test]
    fn bootstrap_clock_is_idempotent() {
        let dir = TestDir::create("bootstrap-idempotent");
        let store = GenerationStore::open(&dir.db_path()).expect("store should open");

        store.bootstrap_clock("rev-1").expect("first bootstrap ok");
        store
            .bootstrap_clock("rev-should-be-ignored")
            .expect("second bootstrap should be a no-op, not an error");

        let revision = store
            .current_workspace_revision()
            .expect("lookup should succeed")
            .expect("clock should be bootstrapped");
        assert_eq!(revision, "rev-1");
    }

    #[test]
    fn set_current_workspace_revision_requires_bootstrap() {
        let dir = TestDir::create("set-without-bootstrap");
        let store = GenerationStore::open(&dir.db_path()).expect("store should open");

        let error = store
            .set_current_workspace_revision("rev-2")
            .expect_err("setting the revision without a bootstrapped clock must be rejected");
        assert!(matches!(error, GenerationError::ClockNotBootstrapped));
    }

    #[test]
    fn reconcile_aborts_orphaned_building_generations_without_touching_stable() {
        let dir = TestDir::create("reconcile-orphan");
        let mut store = GenerationStore::open(&dir.db_path()).expect("store should open");
        store.bootstrap_clock("rev-1").expect("bootstrap ok");

        let stable = store.begin_generation("rev-1").expect("begin stable ok");
        store
            .publish_stable(stable.id)
            .expect("publish should succeed");

        // Simulates a daemon crash mid-build: this generation never
        // published and is now orphaned.
        let orphan = store.begin_generation("rev-1").expect("begin orphan ok");

        let reconciled = store
            .reconcile_orphan_generations()
            .expect("reconcile should succeed");
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].id, orphan.id);
        assert_eq!(reconciled[0].state, GenerationState::Aborted);

        let reloaded_orphan = store
            .get_generation(orphan.id)
            .expect("lookup should succeed")
            .expect("orphan generation should still exist");
        assert_eq!(reloaded_orphan.state, GenerationState::Aborted);

        let current = store
            .current_stable()
            .expect("current lookup should succeed")
            .expect("the original stable generation must remain current");
        assert_eq!(current.id, stable.id);
    }

    #[test]
    fn reconcile_is_a_no_op_when_nothing_is_orphaned() {
        let dir = TestDir::create("reconcile-clean");
        let store = GenerationStore::open(&dir.db_path()).expect("store should open");

        let reconciled = store
            .reconcile_orphan_generations()
            .expect("reconcile should succeed on an empty index.db");
        assert!(reconciled.is_empty());
    }

    #[test]
    fn reopen_then_reconcile_never_exposes_orphan_building_as_current() {
        let dir = TestDir::create("reopen-reconcile");
        {
            let store = GenerationStore::open(&dir.db_path()).expect("store should open");
            store.bootstrap_clock("rev-1").expect("bootstrap ok");
            store.begin_generation("rev-1").expect("begin ok");
            // Process "crashes" here: never published, never reconciled.
        }

        let reopened = GenerationStore::open(&dir.db_path()).expect("store should reopen");
        assert!(
            reopened
                .current_stable()
                .expect("current lookup should succeed")
                .is_none(),
            "an orphaned BUILDING generation must never be current, reconciled or not"
        );

        let reconciled = reopened
            .reconcile_orphan_generations()
            .expect("reconcile should succeed");
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].state, GenerationState::Aborted);
    }
}
