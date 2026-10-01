//! Managed verification Jobs (#54 step 1): the durable record of one
//! accepted verification request in its Workspace's `workspace.db`, and
//! its append-only event log.
//!
//! A Job's state is the runner's lifecycle, not the commands' outcome:
//! `FINISHED` means the batch ran to its end -- a command may still have
//! failed, timed out or not started, and the rest been skipped. Command
//! outcomes are event facts.
//!
//! Stored: the caller's idempotency key, a SHA-256 [`RequestFingerprint`]
//! of the request, and caller-built event payloads. Never stored: argv,
//! env, cwd or command output. The fingerprint is an equality check, not
//! a protection of the values it was computed from.

use std::{fmt, path::Path};

use brainprint_core::VerificationJobId;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::{
    db::{self, DbOpenError},
    diagnostics::DiagnosticFormat,
    output_capture::CaptureRequest,
    schema,
    verification::VerificationCommand,
};

/// The longest idempotency key.
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;
/// The most events one [`VerificationJobStore::events_after`] returns.
pub const MAX_EVENTS_PER_READ: usize = 64;
/// The request encoding [`request_fingerprint`] hashes. Bumped only when
/// that encoding changes -- never with the protocol version.
pub const MANAGED_VERIFICATION_FINGERPRINT_VERSION: u32 = 1;

/// A caller's idempotency key: 1..=128 of `[A-Za-z0-9._:-]`. Unique
/// within one Workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidIdempotencyKey;

impl fmt::Display for InvalidIdempotencyKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "idempotency key must be 1..={MAX_IDEMPOTENCY_KEY_BYTES} of [A-Za-z0-9._:-]"
        )
    }
}

impl std::error::Error for InvalidIdempotencyKey {}

impl IdempotencyKey {
    /// The one validator of the grammar.
    pub fn parse(key: &str) -> Result<Self, InvalidIdempotencyKey> {
        let valid = (1..=MAX_IDEMPOTENCY_KEY_BYTES).contains(&key.len())
            && key.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-')
            });
        if valid {
            Ok(Self(key.to_owned()))
        } else {
            Err(InvalidIdempotencyKey)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// SHA-256 of a request's exact semantic input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestFingerprint(pub [u8; 32]);

/// Fingerprint of a verification request: every field of every command,
/// in order, each variable-length value tagged and length-prefixed so no
/// two different requests encode alike. env keeps its supplied order.
#[must_use]
pub fn request_fingerprint(commands: &[VerificationCommand]) -> RequestFingerprint {
    let mut hasher = Sha256::new();
    hasher.update(b"brainprint.managed-verification.request\0");
    hasher.update(MANAGED_VERIFICATION_FINGERPRINT_VERSION.to_le_bytes());
    count(&mut hasher, b'C', commands.len());
    for command in commands {
        // Destructured in full: a new field cannot be left out silently.
        let VerificationCommand {
            label,
            argv,
            cwd,
            env,
            timeout_secs,
            capture,
        } = command;
        field(&mut hasher, b'l', label.as_bytes());
        count(&mut hasher, b'A', argv.len());
        for arg in argv {
            field(&mut hasher, b'a', arg.as_bytes());
        }
        match cwd {
            None => hasher.update([b'd', 0]),
            Some(cwd) => {
                hasher.update([b'd', 1]);
                field(&mut hasher, b'c', cwd.as_bytes());
            }
        }
        count(&mut hasher, b'E', env.len());
        for (key, value) in env {
            field(&mut hasher, b'k', key.as_bytes());
            field(&mut hasher, b'v', value.as_bytes());
        }
        hasher.update(b"t");
        hasher.update(timeout_secs.to_le_bytes());
        match capture {
            None => hasher.update([b'p', 0]),
            Some(CaptureRequest { raw, diagnostics }) => {
                hasher.update([b'p', 1, b'r', u8::from(*raw), b'f']);
                hasher.update([match diagnostics {
                    None => 0,
                    Some(DiagnosticFormat::CargoCompilerMessageJson) => 1,
                    Some(DiagnosticFormat::PathLineColumn) => 2,
                }]);
            }
        }
    }
    RequestFingerprint(hasher.finalize().into())
}

fn count(hasher: &mut Sha256, tag: u8, count: usize) {
    hasher.update([tag]);
    hasher.update((count as u64).to_le_bytes());
}

fn field(hasher: &mut Sha256, tag: u8, bytes: &[u8]) {
    count(hasher, tag, bytes.len());
    hasher.update(bytes);
}

/// A Job's runner lifecycle. Every state but `Running` is terminal and
/// never left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationJobState {
    Running,
    /// The batch ran to its end, whatever the commands' outcomes.
    Finished,
    /// The caller's explicit cancel ended it.
    Cancelled,
    /// Brainprint lost it: daemon shutdown, restart or crash.
    Interrupted,
    /// A runner/daemon failure left no report.
    InternalError,
}

impl VerificationJobState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "RUNNING",
            Self::Finished => "FINISHED",
            Self::Cancelled => "CANCELLED",
            Self::Interrupted => "INTERRUPTED",
            Self::InternalError => "INTERNAL_ERROR",
        }
    }

    fn parse(raw: &str) -> Result<Self, VerificationJobError> {
        Ok(match raw {
            "RUNNING" => Self::Running,
            "FINISHED" => Self::Finished,
            "CANCELLED" => Self::Cancelled,
            "INTERRUPTED" => Self::Interrupted,
            "INTERNAL_ERROR" => Self::InternalError,
            _ => return Err(VerificationJobError::Corrupt("verification_job.state")),
        })
    }
}

/// The terminal states a running Job can move to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalState {
    Finished,
    Cancelled,
    Interrupted,
    InternalError,
}

impl TerminalState {
    const fn state(self) -> VerificationJobState {
        match self {
            Self::Finished => VerificationJobState::Finished,
            Self::Cancelled => VerificationJobState::Cancelled,
            Self::Interrupted => VerificationJobState::Interrupted,
            Self::InternalError => VerificationJobState::InternalError,
        }
    }

    const fn event(self) -> VerificationJobEventKind {
        match self {
            Self::Finished => VerificationJobEventKind::JobFinished,
            Self::Cancelled => VerificationJobEventKind::JobCancelled,
            Self::Interrupted => VerificationJobEventKind::JobInterrupted,
            Self::InternalError => VerificationJobEventKind::JobInternalError,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationJobEventKind {
    JobStarted,
    CommandStarted,
    CommandFinished,
    JobFinished,
    JobCancelled,
    JobInterrupted,
    JobInternalError,
}

impl VerificationJobEventKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::JobStarted => "JOB_STARTED",
            Self::CommandStarted => "COMMAND_STARTED",
            Self::CommandFinished => "COMMAND_FINISHED",
            Self::JobFinished => "JOB_FINISHED",
            Self::JobCancelled => "JOB_CANCELLED",
            Self::JobInterrupted => "JOB_INTERRUPTED",
            Self::JobInternalError => "JOB_INTERNAL_ERROR",
        }
    }

    fn parse(raw: &str) -> Result<Self, VerificationJobError> {
        Ok(match raw {
            "JOB_STARTED" => Self::JobStarted,
            "COMMAND_STARTED" => Self::CommandStarted,
            "COMMAND_FINISHED" => Self::CommandFinished,
            "JOB_FINISHED" => Self::JobFinished,
            "JOB_CANCELLED" => Self::JobCancelled,
            "JOB_INTERRUPTED" => Self::JobInterrupted,
            "JOB_INTERNAL_ERROR" => Self::JobInternalError,
            _ => return Err(VerificationJobError::Corrupt("verification_job_event.kind")),
        })
    }
}

/// The events a running Job may append between its start and its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressEvent {
    CommandStarted,
    CommandFinished,
}

impl ProgressEvent {
    const fn kind(self) -> VerificationJobEventKind {
        match self {
            Self::CommandStarted => VerificationJobEventKind::CommandStarted,
            Self::CommandFinished => VerificationJobEventKind::CommandFinished,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationJob {
    pub uid: VerificationJobId,
    pub idempotency_key: IdempotencyKey,
    pub request_fingerprint: RequestFingerprint,
    pub state: VerificationJobState,
    pub command_count: u8,
    pub final_summary: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
}

/// One event. `payload_json` is the caller's compact, structured record
/// (never raw output, argv, env or cwd).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationJobEvent {
    pub seq: u64,
    pub kind: VerificationJobEventKind,
    pub payload_json: String,
    pub created_at: String,
}

pub struct NewVerificationJob<'a> {
    pub uid: VerificationJobId,
    pub idempotency_key: &'a IdempotencyKey,
    pub request_fingerprint: RequestFingerprint,
    /// 1..=16.
    pub command_count: u8,
    /// The `JOB_STARTED` event's payload.
    pub started_payload_json: &'a str,
}

/// What a terminal transition did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// The Job was running; it is now terminal, its terminal event at `seq`.
    Applied { seq: u64 },
    /// The Job had already ended in this state; nothing changed.
    AlreadyTerminal(VerificationJobState),
}

#[derive(Debug)]
pub enum VerificationJobError {
    Open(DbOpenError),
    Sqlite(rusqlite::Error),
    /// A Job with this idempotency key exists; it was not touched.
    IdempotencyKeyExists,
    NotFound,
    /// A progress event for a Job that already ended.
    NotRunning(VerificationJobState),
    InvalidLimit(usize),
    /// A stored value outside its shape.
    Corrupt(&'static str),
}

impl fmt::Display for VerificationJobError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(error) => write!(formatter, "workspace.db open failed: {error}"),
            Self::Sqlite(error) => write!(formatter, "verification job storage failed: {error}"),
            Self::IdempotencyKeyExists => {
                formatter.write_str("a verification job with this idempotency key exists")
            }
            Self::NotFound => formatter.write_str("verification job not found"),
            Self::NotRunning(state) => {
                write!(formatter, "verification job is {}", state.as_str())
            }
            Self::InvalidLimit(limit) => {
                write!(formatter, "limit {limit} is not 1..={MAX_EVENTS_PER_READ}")
            }
            Self::Corrupt(what) => write!(formatter, "corrupt {what}"),
        }
    }
}

impl std::error::Error for VerificationJobError {}

impl From<rusqlite::Error> for VerificationJobError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<DbOpenError> for VerificationJobError {
    fn from(error: DbOpenError) -> Self {
        Self::Open(error)
    }
}

macro_rules! job_columns {
    () => {
        "uid, idempotency_key, request_fingerprint, state, command_count, final_summary, \
         created_at, updated_at, finished_at"
    };
}
const JOB_BY_UID: &str = concat!(
    "SELECT ",
    job_columns!(),
    " FROM verification_job WHERE uid = ?1"
);
const JOB_BY_KEY: &str = concat!(
    "SELECT ",
    job_columns!(),
    " FROM verification_job WHERE idempotency_key = ?1"
);
const RUNNING_JOBS: &str = "SELECT id FROM verification_job WHERE state = 'RUNNING'";
const EVENTS_AFTER: &str = "SELECT seq, kind, payload_json, created_at \
    FROM verification_job_event WHERE job_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3";
const NEXT_SEQ: &str =
    "SELECT COALESCE(MAX(seq), 0) + 1 FROM verification_job_event WHERE job_id = ?1";

fn decode_job(row: &Row<'_>) -> Result<VerificationJob, VerificationJobError> {
    let uid: Vec<u8> = row.get(0)?;
    let fingerprint: Vec<u8> = row.get(2)?;
    Ok(VerificationJob {
        uid: VerificationJobId::from_bytes(
            uid.try_into()
                .map_err(|_| VerificationJobError::Corrupt("verification_job.uid"))?,
        ),
        idempotency_key: IdempotencyKey::parse(&row.get::<_, String>(1)?)
            .map_err(|_| VerificationJobError::Corrupt("verification_job.idempotency_key"))?,
        request_fingerprint: RequestFingerprint(
            fingerprint.try_into().map_err(|_| {
                VerificationJobError::Corrupt("verification_job.request_fingerprint")
            })?,
        ),
        state: VerificationJobState::parse(&row.get::<_, String>(3)?)?,
        command_count: row.get(4)?,
        final_summary: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        finished_at: row.get(8)?,
    })
}

/// One Workspace's managed verification Jobs. Every write is one
/// IMMEDIATE transaction, so concurrent writers on the same workspace.db
/// serialize and an event's `seq` is decided inside it.
pub struct VerificationJobStore {
    connection: Connection,
}

impl VerificationJobStore {
    pub fn open(path: &Path) -> Result<Self, VerificationJobError> {
        Ok(Self::from_connection(
            schema::workspace::open(path)?.connection,
        ))
    }

    #[must_use]
    pub fn from_connection(connection: Connection) -> Self {
        Self { connection }
    }

    /// A RUNNING Job and its `JOB_STARTED` event (seq 1), atomically. An
    /// existing key is [`VerificationJobError::IdempotencyKeyExists`]: the
    /// stored Job is never replaced.
    pub fn create(
        &mut self,
        new: &NewVerificationJob<'_>,
    ) -> Result<VerificationJob, VerificationJobError> {
        let now = db::now_millis_text();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key_exists = transaction
            .query_row(
                "SELECT 1 FROM verification_job WHERE idempotency_key = ?1",
                [new.idempotency_key.as_str()],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if key_exists {
            return Err(VerificationJobError::IdempotencyKeyExists);
        }
        transaction.execute(
            concat!(
                "INSERT INTO verification_job (",
                job_columns!(),
                ") VALUES (?1, ?2, ?3, 'RUNNING', ?4, NULL, ?5, ?5, NULL)"
            ),
            params![
                new.uid.to_bytes().to_vec(),
                new.idempotency_key.as_str(),
                new.request_fingerprint.0.to_vec(),
                new.command_count,
                now,
            ],
        )?;
        let id = transaction.last_insert_rowid();
        insert_event(
            &transaction,
            id,
            1,
            VerificationJobEventKind::JobStarted,
            new.started_payload_json,
            &now,
        )?;
        transaction.commit()?;
        self.get_by_id(new.uid)?
            .ok_or(VerificationJobError::Corrupt("verification_job insert"))
    }

    pub fn get_by_id(
        &self,
        uid: VerificationJobId,
    ) -> Result<Option<VerificationJob>, VerificationJobError> {
        self.job(JOB_BY_UID, params![uid.to_bytes().to_vec()])
    }

    pub fn get_by_idempotency_key(
        &self,
        key: &IdempotencyKey,
    ) -> Result<Option<VerificationJob>, VerificationJobError> {
        self.job(JOB_BY_KEY, params![key.as_str()])
    }

    fn job(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Option<VerificationJob>, VerificationJobError> {
        let mut statement = self.connection.prepare_cached(sql)?;
        let mut rows = statement.query(params)?;
        rows.next()?.map(decode_job).transpose()
    }

    /// Append a progress event to a running Job; its `seq`.
    pub fn append(
        &mut self,
        uid: VerificationJobId,
        event: ProgressEvent,
        payload_json: &str,
    ) -> Result<u64, VerificationJobError> {
        let now = db::now_millis_text();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (id, state) = id_and_state(&transaction, uid)?;
        if state != VerificationJobState::Running {
            return Err(VerificationJobError::NotRunning(state));
        }
        let seq = next_seq(&transaction, id)?;
        insert_event(&transaction, id, seq, event.kind(), payload_json, &now)?;
        transaction.execute(
            "UPDATE verification_job SET updated_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        transaction.commit()?;
        Ok(seq as u64)
    }

    /// RUNNING → `to`, with its terminal event, atomically. A Job already
    /// terminal keeps its state and gets no event.
    pub fn finish(
        &mut self,
        uid: VerificationJobId,
        to: TerminalState,
        final_summary: Option<&str>,
        payload_json: &str,
    ) -> Result<Transition, VerificationJobError> {
        let now = db::now_millis_text();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (id, state) = id_and_state(&transaction, uid)?;
        if state != VerificationJobState::Running {
            return Ok(Transition::AlreadyTerminal(state));
        }
        let seq = terminate(&transaction, id, to, final_summary, payload_json, &now)?;
        transaction.commit()?;
        Ok(Transition::Applied { seq: seq as u64 })
    }

    /// Every RUNNING Job → INTERRUPTED, each with one `JOB_INTERRUPTED`
    /// event, in one transaction: for Jobs a previous daemon left running.
    /// Calling it again changes nothing. The Jobs it interrupted.
    pub fn interrupt_all_running(
        &mut self,
        payload_json: &str,
    ) -> Result<Vec<VerificationJobId>, VerificationJobError> {
        let now = db::now_millis_text();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let running: Vec<i64> = transaction
            .prepare(RUNNING_JOBS)?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        let mut interrupted = Vec::with_capacity(running.len());
        for id in running {
            terminate(
                &transaction,
                id,
                TerminalState::Interrupted,
                None,
                payload_json,
                &now,
            )?;
            let uid: Vec<u8> = transaction.query_row(
                "SELECT uid FROM verification_job WHERE id = ?1",
                [id],
                |row| row.get(0),
            )?;
            interrupted
                .push(VerificationJobId::from_bytes(uid.try_into().map_err(
                    |_| VerificationJobError::Corrupt("verification_job.uid"),
                )?));
        }
        transaction.commit()?;
        Ok(interrupted)
    }

    /// Up to `limit` (1..=64) events with `seq > after_seq`, ascending.
    pub fn events_after(
        &self,
        uid: VerificationJobId,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<VerificationJobEvent>, VerificationJobError> {
        if !(1..=MAX_EVENTS_PER_READ).contains(&limit) {
            return Err(VerificationJobError::InvalidLimit(limit));
        }
        let (id, _) = id_and_state(&self.connection, uid)?;
        // seq is at most i64::MAX: a later cursor has nothing after it.
        let Ok(after) = i64::try_from(after_seq) else {
            return Ok(Vec::new());
        };
        let mut statement = self.connection.prepare_cached(EVENTS_AFTER)?;
        let rows = statement.query_map(params![id, after, limit as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get(2)?,
                row.get(3)?,
            ))
        })?;
        rows.map(|row| {
            let (seq, kind, payload_json, created_at) = row?;
            Ok(VerificationJobEvent {
                seq: seq as u64,
                kind: VerificationJobEventKind::parse(&kind)?,
                payload_json,
                created_at,
            })
        })
        .collect()
    }
}

fn id_and_state(
    connection: &Connection,
    uid: VerificationJobId,
) -> Result<(i64, VerificationJobState), VerificationJobError> {
    let found: Option<(i64, String)> = connection
        .query_row(
            "SELECT id, state FROM verification_job WHERE uid = ?1",
            [uid.to_bytes().to_vec()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (id, state) = found.ok_or(VerificationJobError::NotFound)?;
    Ok((id, VerificationJobState::parse(&state)?))
}

fn next_seq(connection: &Connection, id: i64) -> Result<i64, VerificationJobError> {
    Ok(connection.query_row(NEXT_SEQ, [id], |row| row.get(0))?)
}

/// The terminal update and its event, inside the caller's transaction.
fn terminate(
    connection: &Connection,
    id: i64,
    to: TerminalState,
    final_summary: Option<&str>,
    payload_json: &str,
    now: &str,
) -> Result<i64, VerificationJobError> {
    connection.execute(
        "UPDATE verification_job SET state = ?2, final_summary = ?3, updated_at = ?4, \
         finished_at = ?4 WHERE id = ?1 AND state = 'RUNNING'",
        params![id, to.state().as_str(), final_summary, now],
    )?;
    let seq = next_seq(connection, id)?;
    insert_event(connection, id, seq, to.event(), payload_json, now)?;
    Ok(seq)
}

fn insert_event(
    connection: &Connection,
    id: i64,
    seq: i64,
    kind: VerificationJobEventKind,
    payload_json: &str,
    now: &str,
) -> Result<(), VerificationJobError> {
    connection.execute(
        "INSERT INTO verification_job_event (job_id, seq, kind, payload_json, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, seq, kind.as_str(), payload_json, now],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
