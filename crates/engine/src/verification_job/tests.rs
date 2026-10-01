use std::{
    env, fs,
    path::PathBuf,
    process,
    sync::atomic::{AtomicU64, Ordering},
    thread,
};

use rusqlite::{Connection, ErrorCode};

use super::*;
use crate::db::DbKind;

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "brainprint-verification-job-{label}-{}-{sequence}",
            process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("test directory should be created");
        Self(path)
    }

    fn db(&self) -> PathBuf {
        self.0.join("workspace.db")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn key(text: &str) -> IdempotencyKey {
    IdempotencyKey::parse(text).expect("valid key")
}

fn create(store: &mut VerificationJobStore, key_text: &str) -> VerificationJob {
    store
        .create(&NewVerificationJob {
            uid: VerificationJobId::generate(),
            idempotency_key: &key(key_text),
            request_fingerprint: RequestFingerprint([7; 32]),
            command_count: 2,
            started_payload_json: "{}",
        })
        .expect("create")
}

fn all_events(store: &VerificationJobStore, uid: VerificationJobId) -> Vec<VerificationJobEvent> {
    store
        .events_after(uid, 0, MAX_EVENTS_PER_READ)
        .expect("events")
}

fn kinds(events: &[VerificationJobEvent]) -> Vec<(u64, VerificationJobEventKind)> {
    events.iter().map(|event| (event.seq, event.kind)).collect()
}

fn is_constraint(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(inner, _) if inner.code == ErrorCode::ConstraintViolation)
}

fn count(connection: &Connection, table: &str) -> i64 {
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count")
}

// --------------------------------------------------------- idempotency key

#[test]
fn idempotency_keys_follow_the_one_grammar() {
    for valid in [
        "550e8400-e29b-41d4-a716-446655440000",
        "agent42:test:0007",
        "run_1.build-2",
        "x",
        &"k".repeat(128),
    ] {
        assert_eq!(key(valid).as_str(), valid);
    }
    for invalid in [
        "",
        "has space",
        "a/b",
        "a\\b",
        "é",
        "tab\t",
        &"k".repeat(129),
    ] {
        assert_eq!(IdempotencyKey::parse(invalid), Err(InvalidIdempotencyKey));
    }
}

// ------------------------------------------------------------- fingerprint

fn request() -> Vec<VerificationCommand> {
    vec![
        VerificationCommand {
            label: "test".to_owned(),
            argv: vec!["cargo".to_owned(), "test".to_owned(), "--all".to_owned()],
            cwd: Some("crates/engine".to_owned()),
            env: vec![
                ("A".to_owned(), "1".to_owned()),
                ("B".to_owned(), "2".to_owned()),
            ],
            timeout_secs: 600,
            capture: Some(CaptureRequest {
                raw: true,
                diagnostics: Some(DiagnosticFormat::CargoCompilerMessageJson),
            }),
        },
        VerificationCommand {
            label: "lint".to_owned(),
            argv: vec!["cargo".to_owned(), "clippy".to_owned()],
            cwd: None,
            env: Vec::new(),
            timeout_secs: 300,
            capture: None,
        },
    ]
}

#[test]
fn every_semantic_field_changes_the_fingerprint() {
    let base = request_fingerprint(&request());
    assert_eq!(request_fingerprint(&request()), base, "deterministic");
    assert_eq!(MANAGED_VERIFICATION_FINGERPRINT_VERSION, 1);

    type Edit = fn(&mut Vec<VerificationCommand>);
    let edits: Vec<(&str, Edit)> = vec![
        ("label", |r| r[0].label.push('x')),
        ("argv program", |r| r[0].argv[0] = "cargo2".to_owned()),
        ("argv argument", |r| r[0].argv[2] = "--workspace".to_owned()),
        ("argv order", |r| r[0].argv.swap(1, 2)),
        ("argv added", |r| r[1].argv.push(String::new())),
        ("cwd value", |r| r[0].cwd = Some("crates".to_owned())),
        ("cwd none", |r| r[0].cwd = None),
        ("cwd some empty", |r| r[1].cwd = Some(String::new())),
        ("env key", |r| r[0].env[0].0 = "C".to_owned()),
        ("env value", |r| r[0].env[1].1 = "3".to_owned()),
        ("env order", |r| r[0].env.swap(0, 1)),
        ("timeout", |r| r[0].timeout_secs = 601),
        ("capture raw", |r| {
            r[0].capture = Some(CaptureRequest {
                raw: false,
                diagnostics: Some(DiagnosticFormat::CargoCompilerMessageJson),
            });
        }),
        ("diagnostic format", |r| {
            r[0].capture = Some(CaptureRequest {
                raw: true,
                diagnostics: Some(DiagnosticFormat::PathLineColumn),
            });
        }),
        ("capture none", |r| r[0].capture = None),
        ("command order", |r| r.swap(0, 1)),
        ("command count", |r| {
            r.pop();
        }),
    ];
    let mut seen = vec![base];
    for (what, edit) in edits {
        let mut changed = request();
        edit(&mut changed);
        let fingerprint = request_fingerprint(&changed);
        assert!(!seen.contains(&fingerprint), "{what} did not change it");
        seen.push(fingerprint);
    }
}

#[test]
fn field_boundaries_are_part_of_the_encoding() {
    let with_argv = |argv: &[&str]| {
        let mut commands = request();
        commands[1].argv = argv.iter().map(|arg| (*arg).to_owned()).collect();
        request_fingerprint(&commands)
    };
    assert_ne!(with_argv(&["ab", "c"]), with_argv(&["a", "bc"]));
    assert_ne!(with_argv(&["abc"]), with_argv(&["ab", "c"]));
    let with_env = |env: &[(&str, &str)]| {
        let mut commands = request();
        commands[1].env = env
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        request_fingerprint(&commands)
    };
    assert_ne!(with_env(&[("AB", "C")]), with_env(&[("A", "BC")]));
    // The same text as a label or an argument is not the same request.
    let mut label = request();
    label[1].label = "cargo".to_owned();
    label[1].argv = vec!["lint".to_owned()];
    let mut argv = request();
    argv[1].argv = vec!["lint".to_owned()];
    assert_ne!(request_fingerprint(&label), request_fingerprint(&argv));
}

// ------------------------------------------------------------------ schema

fn index_on(connection: &Connection, table: &str, column: &str) -> String {
    connection
        .query_row(
            "SELECT l.name FROM pragma_index_list(?1) l, pragma_index_info(l.name) i \
             WHERE i.name = ?2 AND (SELECT COUNT(*) FROM pragma_index_info(l.name)) = 1",
            [table, column],
            |row| row.get(0),
        )
        .unwrap_or_else(|error| panic!("no index on {table}.{column}: {error}"))
}

fn table_exists(connection: &Connection, table: &str) -> bool {
    connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get::<_, i64>(0),
        )
        .expect("sqlite_master")
        == 1
}

#[test]
fn a_fresh_workspace_db_has_the_job_tables() {
    let dir = TestDir::create("fresh");
    let opened = schema::workspace::open(&dir.db()).expect("v6");
    let connection = &opened.connection;
    assert!(table_exists(connection, "verification_job"));
    assert!(table_exists(connection, "verification_job_event"));
    assert_eq!(
        index_on(connection, "verification_job", "state"),
        "idx_verification_job_state"
    );
    index_on(connection, "verification_job", "uid");
    index_on(connection, "verification_job", "idempotency_key");
    let max: i64 = connection
        .query_row("SELECT MAX(version) FROM schema_migration", [], |row| {
            row.get(0)
        })
        .expect("version");
    assert_eq!(max, 6);
}

#[test]
fn v5_upgrades_with_every_row_and_binding_kept() {
    let dir = TestDir::create("upgrade");
    let workspace_uid = vec![9_u8; 16];
    let tables = [
        "work_item",
        "working_state",
        "work_result",
        "work_note",
        "workspace_project_state",
    ];
    let before: Vec<i64> = {
        let v5 = db::open(
            &dir.db(),
            DbKind::Workspace,
            &schema::workspace::WORKSPACE_MIGRATIONS[..5],
        )
        .expect("v5");
        let connection = &v5.connection;
        connection
            .execute_batch(
                "INSERT INTO work_item (uid, source_kind, goal, status, created_at)
                     VALUES (x'01010101010101010101010101010101', 'MANUAL', 'g', 'ACTIVE', '0');
                 INSERT INTO working_state (work_item_id, baseline_workspace_revision,
                     baseline_generation_no, last_observed_workspace_revision, updated_at)
                     VALUES (1, 'r', 1, 'r', '0');
                 INSERT INTO work_result (work_item_id, result_status, result_summary,
                     result_workspace_revision, created_at)
                     VALUES (1, 'PARTIAL', 's', 'r', '0');
                 INSERT INTO work_note (uid, work_item_id, kind, note_text, status,
                     source_kind, created_at, updated_at)
                     VALUES (x'02020202020202020202020202020202', 1, 'OBSERVATION', 'n',
                             'OPEN', 'AGENT', '0', '0');
                 INSERT INTO workspace_project_state (uid, state_key, scope_kind, value_type,
                     value_json, status, source_kind, updated_at)
                     VALUES (x'03030303030303030303030303030303', 'k', 'PROJECT', 'STRING',
                             '\"v\"', 'ACTIVE', 'AGENT', '0');",
            )
            .expect("v5 rows");
        connection
            .execute(
                "UPDATE db_meta SET workspace_uid = ?1 WHERE id = 0",
                [&workspace_uid],
            )
            .expect("binding");
        tables
            .iter()
            .map(|table| count(connection, table))
            .collect()
    };
    assert!(before.iter().all(|&rows| rows == 1));

    let migrations = |connection: &Connection| -> Vec<(i64, String)> {
        connection
            .prepare("SELECT version, name FROM schema_migration ORDER BY version")
            .expect("prepare")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    };
    for _ in 0..2 {
        // The second open is a reopen: nothing runs again.
        let opened = schema::workspace::open(&dir.db()).expect("v6");
        let connection = &opened.connection;
        let after: Vec<i64> = tables
            .iter()
            .map(|table| count(connection, table))
            .collect();
        assert_eq!(after, before);
        assert_eq!(count(connection, "verification_job"), 0);
        assert_eq!(count(connection, "verification_job_event"), 0);
        let applied = migrations(connection);
        assert_eq!(applied.len(), 6);
        assert_eq!(
            applied[5],
            (6, "create_managed_verification_jobs".to_owned())
        );
        let bound: Vec<u8> = connection
            .query_row(
                "SELECT workspace_uid FROM db_meta WHERE id = 0",
                [],
                |row| row.get(0),
            )
            .expect("binding");
        assert_eq!(bound, workspace_uid);
    }
    // Still a workspace.db, never openable as another kind.
    assert!(matches!(
        db::open(
            &dir.db(),
            DbKind::Project,
            schema::project::PROJECT_MIGRATIONS
        ),
        Err(DbOpenError::KindMismatch { .. })
    ));
}

#[test]
fn sqlite_rejects_what_the_schema_rules_out() {
    let dir = TestDir::create("constraints");
    let opened = schema::workspace::open(&dir.db()).expect("v6");
    let connection = &opened.connection;
    let insert = |uid: Vec<u8>,
                  key: &str,
                  fingerprint: Vec<u8>,
                  state: &str,
                  commands: i64,
                  finished: Option<&str>| {
        connection.execute(
            "INSERT INTO verification_job (uid, idempotency_key, request_fingerprint, state, \
             command_count, created_at, updated_at, finished_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, '0', '0', ?6)",
            params![uid, key, fingerprint, state, commands, finished],
        )
    };
    insert(vec![1; 16], "ok", vec![0; 32], "RUNNING", 1, None).expect("a valid row");
    let cases: Vec<(&str, rusqlite::Result<usize>)> = vec![
        (
            "uid 15 bytes",
            insert(vec![2; 15], "a", vec![0; 32], "RUNNING", 1, None),
        ),
        (
            "uid 17 bytes",
            insert(vec![2; 17], "b", vec![0; 32], "RUNNING", 1, None),
        ),
        (
            "uid as text",
            connection.execute(
                "INSERT INTO verification_job (uid, idempotency_key, request_fingerprint, state, \
             command_count, created_at, updated_at) \
             VALUES ('0123456789abcdef', 'c', zeroblob(32), 'RUNNING', 1, '0', '0')",
                [],
            ),
        ),
        (
            "fingerprint 31",
            insert(vec![3; 16], "d", vec![0; 31], "RUNNING", 1, None),
        ),
        (
            "fingerprint 33",
            insert(vec![4; 16], "e", vec![0; 33], "RUNNING", 1, None),
        ),
        (
            "empty key",
            insert(vec![5; 16], "", vec![0; 32], "RUNNING", 1, None),
        ),
        (
            "129-char key",
            insert(
                vec![6; 16],
                &"k".repeat(129),
                vec![0; 32],
                "RUNNING",
                1,
                None,
            ),
        ),
        (
            "unknown state",
            insert(vec![7; 16], "f", vec![0; 32], "DONE", 1, None),
        ),
        (
            "0 commands",
            insert(vec![8; 16], "g", vec![0; 32], "RUNNING", 0, None),
        ),
        (
            "17 commands",
            insert(vec![9; 16], "h", vec![0; 32], "RUNNING", 17, None),
        ),
        (
            "running + finished_at",
            insert(vec![10; 16], "i", vec![0; 32], "RUNNING", 1, Some("1")),
        ),
        (
            "terminal without finished_at",
            insert(vec![11; 16], "j", vec![0; 32], "FINISHED", 1, None),
        ),
        (
            "duplicate uid",
            insert(vec![1; 16], "k", vec![0; 32], "RUNNING", 1, None),
        ),
        (
            "duplicate key",
            insert(vec![12; 16], "ok", vec![0; 32], "RUNNING", 1, None),
        ),
    ];
    for (what, result) in cases {
        let error = result.expect_err(what);
        assert!(is_constraint(&error), "{what}: {error}");
    }
    insert(
        vec![13; 16],
        "done",
        vec![0; 32],
        "INTERRUPTED",
        16,
        Some("1"),
    )
    .expect("a terminal row with finished_at");

    let event = |job: i64, seq: i64, kind: &str| {
        connection.execute(
            "INSERT INTO verification_job_event (job_id, seq, kind, payload_json, created_at) \
             VALUES (?1, ?2, ?3, '{}', '0')",
            params![job, seq, kind],
        )
    };
    event(1, 1, "JOB_STARTED").expect("a valid event");
    for (what, result) in [
        ("duplicate (job_id, seq)", event(1, 1, "COMMAND_STARTED")),
        ("nonexistent job", event(999, 1, "JOB_STARTED")),
        ("unknown kind", event(1, 2, "COMMAND_OUTPUT")),
        ("seq 0", event(1, 0, "COMMAND_STARTED")),
    ] {
        let error = result.expect_err(what);
        assert!(is_constraint(&error), "{what}: {error}");
    }
}

// --------------------------------------------------------------- the store

#[test]
fn a_job_runs_through_its_events_to_one_terminal_state() {
    let dir = TestDir::create("lifecycle");
    let mut store = VerificationJobStore::open(&dir.db()).expect("open");
    let job = create(&mut store, "run:1");
    assert_eq!(job.state, VerificationJobState::Running);
    assert_eq!(job.finished_at, None);
    assert_eq!(job.command_count, 2);
    assert_eq!(store.get_by_id(job.uid).expect("get"), Some(job.clone()));
    assert_eq!(
        store.get_by_idempotency_key(&key("run:1")).expect("get"),
        Some(job.clone())
    );
    assert_eq!(
        store.get_by_id(VerificationJobId::generate()).expect("get"),
        None
    );
    assert_eq!(
        store.get_by_idempotency_key(&key("other")).expect("get"),
        None
    );

    let payload = r#"{"command":0,"label":"test"}"#;
    assert_eq!(
        store
            .append(job.uid, ProgressEvent::CommandStarted, payload)
            .expect("append"),
        2
    );
    assert_eq!(
        store
            .append(job.uid, ProgressEvent::CommandFinished, payload)
            .expect("append"),
        3
    );
    // A failed command still ends the Job FINISHED: the state is the
    // runner's lifecycle, the outcome an event fact.
    let summary = "test: failed exit=1 10ms; lint: skipped";
    assert_eq!(
        store
            .finish(job.uid, TerminalState::Finished, Some(summary), "{}")
            .expect("finish"),
        Transition::Applied { seq: 4 }
    );
    let finished = store.get_by_id(job.uid).expect("get").expect("job");
    assert_eq!(finished.state, VerificationJobState::Finished);
    assert_eq!(finished.final_summary.as_deref(), Some(summary));
    assert!(finished.finished_at.is_some());

    let events = all_events(&store, job.uid);
    assert_eq!(
        kinds(&events),
        [
            (1, VerificationJobEventKind::JobStarted),
            (2, VerificationJobEventKind::CommandStarted),
            (3, VerificationJobEventKind::CommandFinished),
            (4, VerificationJobEventKind::JobFinished),
        ]
    );
    assert_eq!(events[1].payload_json, payload);

    // Delta reads.
    assert_eq!(
        kinds(&store.events_after(job.uid, 2, 1).expect("delta")),
        [(3, VerificationJobEventKind::CommandFinished)]
    );
    assert!(
        store
            .events_after(job.uid, 4, 64)
            .expect("delta")
            .is_empty()
    );
    assert!(
        store
            .events_after(job.uid, u64::MAX, 64)
            .expect("delta")
            .is_empty()
    );
    for limit in [0, 65] {
        assert!(matches!(
            store.events_after(job.uid, 0, limit),
            Err(VerificationJobError::InvalidLimit(_))
        ));
    }
    assert!(matches!(
        store.events_after(VerificationJobId::generate(), 0, 1),
        Err(VerificationJobError::NotFound)
    ));

    // Ended: no progress, no second terminal state or event.
    assert!(matches!(
        store.append(job.uid, ProgressEvent::CommandStarted, "{}"),
        Err(VerificationJobError::NotRunning(
            VerificationJobState::Finished
        ))
    ));
    assert_eq!(
        store
            .finish(job.uid, TerminalState::Cancelled, None, "{}")
            .expect("finish"),
        Transition::AlreadyTerminal(VerificationJobState::Finished)
    );
    assert_eq!(store.get_by_id(job.uid).expect("get"), Some(finished));
    assert_eq!(all_events(&store, job.uid).len(), 4);
    assert!(matches!(
        store.finish(
            VerificationJobId::generate(),
            TerminalState::Finished,
            None,
            "{}"
        ),
        Err(VerificationJobError::NotFound)
    ));
}

#[test]
fn cancel_first_then_finish_keeps_the_cancel() {
    let dir = TestDir::create("cancel-first");
    let mut store = VerificationJobStore::open(&dir.db()).expect("open");
    let job = create(&mut store, "cancel-first");
    assert_eq!(
        store
            .finish(job.uid, TerminalState::Cancelled, None, "{}")
            .expect("cancel"),
        Transition::Applied { seq: 2 }
    );
    assert_eq!(
        store
            .finish(job.uid, TerminalState::Finished, Some("late"), "{}")
            .expect("finish"),
        Transition::AlreadyTerminal(VerificationJobState::Cancelled)
    );
    let stored = store.get_by_id(job.uid).expect("get").expect("job");
    assert_eq!(
        (stored.state, stored.final_summary),
        (VerificationJobState::Cancelled, None)
    );
    assert_eq!(
        kinds(&all_events(&store, job.uid)),
        [
            (1, VerificationJobEventKind::JobStarted),
            (2, VerificationJobEventKind::JobCancelled)
        ]
    );
}

#[test]
fn a_taken_key_is_refused_and_the_job_left_alone() {
    let dir = TestDir::create("duplicate-key");
    let mut store = VerificationJobStore::open(&dir.db()).expect("open");
    let first = create(&mut store, "same");
    let error = store
        .create(&NewVerificationJob {
            uid: VerificationJobId::generate(),
            idempotency_key: &key("same"),
            request_fingerprint: RequestFingerprint([8; 32]),
            command_count: 1,
            started_payload_json: "{}",
        })
        .expect_err("duplicate key");
    assert!(matches!(error, VerificationJobError::IdempotencyKeyExists));
    assert_eq!(
        store.get_by_idempotency_key(&key("same")).expect("get"),
        Some(first.clone())
    );
    assert_eq!(all_events(&store, first.uid).len(), 1);
    assert_eq!(count(&store.connection, "verification_job"), 1);
}

#[test]
fn a_failed_write_leaves_no_half_job_and_no_silent_terminal() {
    let dir = TestDir::create("atomic");
    let mut store = VerificationJobStore::open(&dir.db()).expect("open");
    let fail_on = |store: &VerificationJobStore, kind: &str| {
        store
            .connection
            .execute_batch(&format!(
                "DROP TRIGGER IF EXISTS fail_event;
                 CREATE TEMP TRIGGER fail_event BEFORE INSERT ON main.verification_job_event
                 WHEN NEW.kind = '{kind}' BEGIN SELECT RAISE(ABORT, 'injected'); END;"
            ))
            .expect("trigger");
    };

    fail_on(&store, "JOB_STARTED");
    let uid = VerificationJobId::generate();
    store
        .create(&NewVerificationJob {
            uid,
            idempotency_key: &key("half"),
            request_fingerprint: RequestFingerprint([1; 32]),
            command_count: 1,
            started_payload_json: "{}",
        })
        .expect_err("injected");
    assert_eq!(store.get_by_id(uid).expect("get"), None);
    assert_eq!(count(&store.connection, "verification_job"), 0);

    fail_on(&store, "JOB_FINISHED");
    let job = create(&mut store, "terminal");
    store
        .finish(job.uid, TerminalState::Finished, Some("s"), "{}")
        .expect_err("injected");
    assert_eq!(store.get_by_id(job.uid).expect("get"), Some(job.clone()));
    assert_eq!(all_events(&store, job.uid).len(), 1);

    fail_on(&store, "JOB_INTERRUPTED");
    store.interrupt_all_running("{}").expect_err("injected");
    assert_eq!(store.get_by_id(job.uid).expect("get"), Some(job));
}

#[test]
fn stale_running_jobs_are_interrupted_once() {
    let dir = TestDir::create("reconcile");
    let mut store = VerificationJobStore::open(&dir.db()).expect("open");
    let running = [create(&mut store, "a"), create(&mut store, "b")];
    store
        .append(running[0].uid, ProgressEvent::CommandStarted, "{}")
        .expect("append");
    let done = create(&mut store, "done");
    store
        .finish(done.uid, TerminalState::Finished, None, "{}")
        .expect("finish");
    let cancelled = create(&mut store, "cancelled");
    store
        .finish(cancelled.uid, TerminalState::Cancelled, None, "{}")
        .expect("cancel");

    let mut interrupted = store
        .interrupt_all_running(r#"{"reason":"restart"}"#)
        .expect("reconcile");
    let mut expected = vec![running[0].uid, running[1].uid];
    interrupted.sort_by_key(|uid| uid.to_bytes());
    expected.sort_by_key(|uid| uid.to_bytes());
    assert_eq!(interrupted, expected);
    for (job, last_seq) in [(&running[0], 3), (&running[1], 2)] {
        let stored = store.get_by_id(job.uid).expect("get").expect("job");
        assert_eq!(stored.state, VerificationJobState::Interrupted);
        assert!(stored.finished_at.is_some());
        let events = all_events(&store, job.uid);
        assert_eq!(
            events.last().map(|event| (event.seq, event.kind)),
            Some((last_seq, VerificationJobEventKind::JobInterrupted))
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == VerificationJobEventKind::JobInterrupted)
                .count(),
            1
        );
    }
    for (job, state) in [
        (&done, VerificationJobState::Finished),
        (&cancelled, VerificationJobState::Cancelled),
    ] {
        assert_eq!(
            store.get_by_id(job.uid).expect("get").expect("job").state,
            state
        );
        assert_eq!(all_events(&store, job.uid).len(), 2);
    }

    let events_before = count(&store.connection, "verification_job_event");
    assert!(store.interrupt_all_running("{}").expect("again").is_empty());
    assert_eq!(
        count(&store.connection, "verification_job_event"),
        events_before
    );
}

#[test]
fn concurrent_appends_never_share_a_seq() {
    let dir = TestDir::create("concurrent");
    let mut store = VerificationJobStore::open(&dir.db()).expect("open");
    let job = create(&mut store, "concurrent").uid;
    let writers: Vec<_> = (0..4)
        .map(|_| {
            let path = dir.db();
            thread::spawn(move || {
                let mut store = VerificationJobStore::open(&path).expect("open");
                (0..15)
                    .map(|_| {
                        store
                            .append(job, ProgressEvent::CommandFinished, "{}")
                            .expect("append")
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut seqs: Vec<u64> = writers
        .into_iter()
        .flat_map(|writer| writer.join().expect("writer"))
        .collect();
    seqs.sort_unstable();
    assert_eq!(seqs, (2..=61).collect::<Vec<_>>());
}

/// No argv, env or cwd text reaches workspace.db: only the fingerprint.
#[test]
fn request_values_are_never_stored() {
    let dir = TestDir::create("no-raw");
    let mut commands = request();
    commands[0].argv.push("SECRET_ARG_MARKER_54".to_owned());
    commands[0].env.push((
        "SECRET_ENV_KEY_54".to_owned(),
        "SECRET_ENV_MARKER_54".to_owned(),
    ));
    commands[0].cwd = Some("SECRET_CWD_MARKER_54".to_owned());
    let fingerprint = request_fingerprint(&commands);
    {
        let mut store = VerificationJobStore::open(&dir.db()).expect("open");
        let job = store
            .create(&NewVerificationJob {
                uid: VerificationJobId::generate(),
                idempotency_key: &key("no-raw"),
                request_fingerprint: fingerprint,
                command_count: 2,
                started_payload_json: r#"{"commands":2}"#,
            })
            .expect("create");
        assert_eq!(job.request_fingerprint, fingerprint);
        store
            .finish(
                job.uid,
                TerminalState::Finished,
                Some("test: passed 1ms"),
                "{}",
            )
            .expect("finish");
    }
    let mut stored = Vec::new();
    for entry in fs::read_dir(&dir.0).expect("dir") {
        stored.extend(fs::read(entry.expect("entry").path()).expect("file"));
    }
    for marker in [
        "SECRET_ARG_MARKER_54",
        "SECRET_ENV_KEY_54",
        "SECRET_ENV_MARKER_54",
        "SECRET_CWD_MARKER_54",
    ] {
        assert!(
            !stored
                .windows(marker.len())
                .any(|at| at == marker.as_bytes()),
            "{marker} is in workspace.db"
        );
    }
}

// -------------------------------------------------------------- query plans

fn plan(connection: &Connection, sql: &str, params: impl rusqlite::Params) -> String {
    connection
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("explain")
        .query_map(params, |row| row.get::<_, String>(3))
        .expect("plan")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows")
        .join(" | ")
}

/// Hundreds of finished Jobs, one running, several events each: every hot
/// and recovery query searches an index; none scans or sorts.
#[test]
fn hot_queries_search_their_index() {
    let dir = TestDir::create("plans");
    let mut store = VerificationJobStore::open(&dir.db()).expect("open");
    let mut last = None;
    for index in 0..300 {
        let job = create(&mut store, &format!("job:{index}"));
        store
            .append(job.uid, ProgressEvent::CommandStarted, "{}")
            .expect("append");
        store
            .append(job.uid, ProgressEvent::CommandFinished, "{}")
            .expect("append");
        store
            .finish(job.uid, TerminalState::Finished, None, "{}")
            .expect("finish");
        last = Some(job);
    }
    let running = create(&mut store, "running");
    store.connection.execute_batch("ANALYZE").expect("analyze");
    let connection = &store.connection;
    let uid_index = index_on(connection, "verification_job", "uid");
    let key_index = index_on(connection, "verification_job", "idempotency_key");
    let job_uid = last.expect("jobs").uid.to_bytes().to_vec();

    let cases = [
        (plan(connection, JOB_BY_UID, [&job_uid]), uid_index),
        (plan(connection, JOB_BY_KEY, ["job:7"]), key_index),
        (
            plan(connection, RUNNING_JOBS, []),
            "idx_verification_job_state".to_owned(),
        ),
        (
            plan(connection, EVENTS_AFTER, params![150_i64, 1_i64, 64_i64]),
            "sqlite_autoindex_verification_job_event_1".to_owned(),
        ),
        (
            plan(connection, NEXT_SEQ, [150_i64]),
            "sqlite_autoindex_verification_job_event_1".to_owned(),
        ),
    ];
    for (detail, index) in cases {
        assert!(
            detail.contains("SEARCH") && detail.contains(&index),
            "{index}: {detail}"
        );
        assert!(!detail.contains("SCAN"), "{detail}");
        assert!(!detail.contains("TEMP B-TREE"), "{detail}");
    }
    // The running one is what the state lookup finds.
    assert_eq!(
        store.interrupt_all_running("{}").expect("reconcile"),
        [running.uid]
    );
}
