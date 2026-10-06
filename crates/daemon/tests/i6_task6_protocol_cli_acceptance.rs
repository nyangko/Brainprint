//! #55 step 3 acceptance: the post-command refresh, its bounded delta and
//! managed Job provenance through protocol 9 and the real CLI.
//!
//! - The dogfood tests spawn the real `brainprintd` and drive it only
//!   with the real `brainprint` binary: a managed `cargo fmt` changes the
//!   Workspace, the terminal poll carries the refresh, the next native
//!   query is already current, and the Job is attached to a Work Result
//!   without running again. Nothing reconciles by hand or sleeps for a
//!   watcher in between; polling for the Job's end is the only wait.
//! - The wire tests use an in-process server so a test can reach its
//!   runtime: a silent scripted watcher (nothing is delivered unless
//!   pushed), or no watcher at all with a hook in the watch factory --
//!   which a watcher-less Workspace calls once per settle on its worker,
//!   so a test can change the Workspace at an exact point of a request.
//!
//! `harness = false`: this test binary is also the portable helper the
//! commands run (no shell). Helper ops, in order: `exit:<code>`,
//! `sleep:<ms>`, `mark:<path>` (append one byte), `await:<path>` (until
//! it exists), `write:<path>` (a small Rust file), `swap:<path>`
//! (`helper` -> `zelper`, same size, mtime put back), `many:<dir>|<n>`
//! (n files with long names), `move:<from>|<to>` (rename).

use std::{
    env, fs,
    future::Future,
    io::Write as _,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process::{self, Child, Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use brainprint_core::{
    IndexIncarnationId, PROTOCOL_VERSION, VerificationJobId, WorkItemId, WorkspaceId,
    protocol::{
        self, ClientConnection, HandshakeRequest, HandshakeResponse, InitRequest, InitResponse,
        Request, Response, framing::MAX_MESSAGE_BYTES, query::*, verification_job::*, work::*,
    },
};
use brainprint_daemon::{
    query::{DaemonQueryRuntime, MAX_REFRESH_DELTA_WIRE_BYTES, lifecycle::WatchFactory},
    runtime_paths,
    server::Server,
};
use brainprint_engine::{
    paths::{GlobalPaths, WorkspacePaths},
    verification::VerificationCommand,
    verification_job::{
        IdempotencyKey, NewVerificationJob, ProgressEvent, TerminalState, VerificationJobStore,
        request_fingerprint,
    },
    watch::{RawWatchEvent, WatchSource},
};
use rusqlite::{Connection, OpenFlags};

const HELPER: &str = "--brainprint-verification-helper";
const LATE_MS: u64 = 3000;
const MISSING: &str = "brainprint-i6-task6-step3-no-such-command";

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(HELPER) {
        helper(&args[1..]);
        return;
    }
    let filter = args.iter().find(|arg| !arg.starts_with('-')).cloned();
    let tests: &[(&str, fn())] = &[
        (
            "dogfood_formatter_refresh_query_and_attach",
            dogfood_formatter_refresh_query_and_attach,
        ),
        (
            "dogfood_same_size_same_mtime_through_poll",
            dogfood_same_size_same_mtime_through_poll,
        ),
        (
            "managed_refresh_wire_and_compact_lines",
            managed_refresh_wire_and_compact_lines,
        ),
        (
            "a_large_delta_keeps_exact_counts_within_the_bounds",
            a_large_delta_keeps_exact_counts_within_the_bounds,
        ),
        (
            "failed_refresh_and_baseline_reason_are_typed",
            failed_refresh_and_baseline_reason_are_typed,
        ),
        (
            "v1_jobs_poll_without_refresh_and_v2_without_one_is_corrupt",
            v1_jobs_poll_without_refresh_and_v2_without_one_is_corrupt,
        ),
        (
            "sync_refresh_facts_over_the_wire",
            sync_refresh_facts_over_the_wire,
        ),
        ("sync_basis_race_is_typed", sync_basis_race_is_typed),
        (
            "attach_over_the_wire_records_or_refuses_without_running",
            attach_over_the_wire_records_or_refuses_without_running,
        ),
        (
            "attach_toctou_over_the_wire_is_stale",
            attach_toctou_over_the_wire_is_stale,
        ),
        (
            "protocol_14_handshakes_strictly",
            protocol_14_handshakes_strictly,
        ),
        (
            "protocol_is_14_schema_7_payload_2",
            protocol_is_14_schema_7_payload_2,
        ),
    ];
    let handles: Vec<_> = tests
        .iter()
        .filter(|(name, _)| filter.as_deref().is_none_or(|filter| name.contains(filter)))
        .map(|&(name, test)| (name, thread::spawn(test)))
        .collect();
    let total = handles.len();
    let mut failed = 0;
    for (name, handle) in handles {
        let ok = handle.join().is_ok();
        println!("test {name} ... {}", if ok { "ok" } else { "FAILED" });
        failed += usize::from(!ok);
    }
    println!("{} passed; {failed} failed", total - failed);
    if failed > 0 {
        process::exit(101);
    }
}

// ---------------------------------------------------------------- helper

fn helper(ops: &[String]) {
    for op in ops {
        let (name, value) = op.split_once(':').expect("op:value");
        match name {
            "exit" => process::exit(value.parse().expect("code")),
            "sleep" => thread::sleep(Duration::from_millis(value.parse().expect("ms"))),
            "mark" => fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(value)
                .and_then(|mut file| file.write_all(b"x"))
                .expect("marker"),
            "await" => {
                let deadline = Instant::now() + Duration::from_secs(60);
                while !Path::new(value).exists() {
                    assert!(Instant::now() < deadline, "never released");
                    thread::sleep(Duration::from_millis(10));
                }
            }
            "write" => {
                let path = Path::new(value);
                fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
                fs::write(path, "pub fn generated() -> i32 {\n    1\n}\n").expect("write");
            }
            "swap" => swap_keeping_size_and_mtime(Path::new(value)),
            "many" => {
                let (dir, count) = value.split_once('|').expect("dir|n");
                fs::create_dir_all(dir).expect("dir");
                for index in 0..count.parse::<usize>().expect("n") {
                    let name = format!("{index:0>96}.txt");
                    fs::write(Path::new(dir).join(name), "x\n").expect("write");
                }
            }
            "move" => {
                let (from, to) = value.split_once('|').expect("from|to");
                fs::rename(from, to).expect("move");
            }
            other => panic!("unknown helper op {other}"),
        }
    }
}

fn swap_keeping_size_and_mtime(path: &Path) {
    let mtime = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .expect("mtime");
    let body = fs::read_to_string(path).expect("read");
    let changed = body.replace("helper", "zelper");
    assert_eq!((changed.len(), changed != body), (body.len(), true));
    fs::write(path, changed).expect("rewrite");
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open")
        .set_modified(mtime)
        .expect("restore mtime");
}

fn command(label: &str, ops: &[&str]) -> VerificationCommandWire {
    let exe = env::current_exe().expect("exe");
    VerificationCommandWire {
        label: label.to_owned(),
        argv: [exe.to_string_lossy().as_ref(), HELPER]
            .into_iter()
            .chain(ops.iter().copied())
            .map(str::to_owned)
            .collect(),
        cwd: None,
        env: Vec::new(),
        timeout_secs: 120,
        capture: None,
    }
}

fn missing() -> VerificationCommandWire {
    VerificationCommandWire {
        argv: vec![MISSING.to_owned()],
        ..command("missing", &[])
    }
}

fn batch(commands: Vec<VerificationCommandWire>) -> VerificationWire {
    VerificationWire { commands }
}

fn op(name: &str, path: &Path) -> String {
    format!("{name}:{}", path.display())
}

fn count(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !ready() {
        assert!(Instant::now() < deadline, "{what} never happened");
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for(path: &Path) {
    wait_until(&format!("{} appearing", path.display()), || path.exists());
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let path = env::temp_dir().join(format!(
            "bp-i6t6s3-{label}-{}-{}",
            process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("test dir");
        Self(path.canonicalize().expect("canonical"))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git");
    assert!(output.status.success(), "git {args:?}");
}

/// A Cargo crate `cargo fmt` really rewrites.
const LIB_RS: &str = "pub fn add(a:i32,b:i32)->i32{a+b}\npub fn helper()->i32{ 42 }\npub fn sub(a:i32,b:i32)->i32{a-b}\n";
const CARGO_TOML: &str =
    "[package]\nname = \"dogfood\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n";

fn crate_tree(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("src");
    fs::write(root.join("Cargo.toml"), CARGO_TOML).expect("Cargo.toml");
    fs::write(root.join("src/lib.rs"), LIB_RS).expect("lib.rs");
    git(root, &["init", "-q"]);
    git(root, &["config", "core.autocrlf", "false"]);
}

// ------------------------------------------------------------------- CLI

fn cli_bin() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_BIN_EXE_brainprintd"));
    path.set_file_name(if cfg!(windows) {
        "brainprint.exe"
    } else {
        "brainprint"
    });
    assert!(
        path.is_file(),
        "build the workspace first: {}",
        path.display()
    );
    path
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The real CLI against one Workspace of one daemon home.
#[derive(Clone)]
struct Cli {
    home: PathBuf,
    workspace: String,
}

impl Cli {
    fn run(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut child = Command::new(cli_bin())
            .args(args)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env_remove("XDG_RUNTIME_DIR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("brainprint");
        let mut input = child.stdin.take().expect("stdin");
        if let Some(text) = stdin {
            input.write_all(text.as_bytes()).expect("stdin");
        }
        drop(input);
        child.wait_with_output().expect("output")
    }

    /// `<args> --workspace <ws> [--json]`.
    fn on(&self, args: &[&str], json: bool, stdin: Option<&str>) -> Output {
        let mut all = args.to_vec();
        all.extend_from_slice(&["--workspace", &self.workspace]);
        if json {
            all.push("--json");
        }
        self.run(&all, stdin)
    }

    fn start(&self, key: &str, verification: &VerificationWire) -> (VerificationJobId, Duration) {
        let input = serde_json::to_string(verification).expect("json");
        let began = Instant::now();
        let output = self.on(
            &[
                "verification",
                "start",
                "--idempotency-key",
                key,
                "--input",
                "-",
            ],
            false,
            Some(&input),
        );
        let returned = began.elapsed();
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        let line = text(&output.stdout);
        let job = line
            .strip_prefix("job ")
            .and_then(|rest| rest.strip_suffix(" running\n"))
            .unwrap_or_else(|| panic!("start printed {line:?}"))
            .parse()
            .expect("job id");
        (job, returned)
    }

    fn poll(&self, job: VerificationJobId) -> VerificationJobPollWire {
        let output = self.on(&["verification", "poll", &job.to_string()], true, None);
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        let response: VerificationJobPollResponseWire =
            serde_json::from_slice(&output.stdout).expect("poll JSON");
        response.expect("polled")
    }

    fn compact_poll(&self, job: VerificationJobId) -> String {
        let output = self.on(&["verification", "poll", &job.to_string()], false, None);
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        text(&output.stdout)
    }

    /// Polls until the Job is terminal: the Job's own lifecycle barrier.
    fn terminal(&self, job: VerificationJobId) -> VerificationJobPollWire {
        let mut done = None;
        wait_until("the Job ending", || {
            let poll = self.poll(job);
            let ended = poll.state != VerificationJobStateWire::Running && !poll.has_more;
            done = ended.then_some(poll);
            ended
        });
        done.expect("terminal")
    }

    fn work(&self, sub: &str, input: &str, json: bool) -> Output {
        self.on(&["work", sub, "--input", "-"], json, Some(input))
    }

    fn started(&self) -> WorkItemId {
        let input = serde_json::to_string(&WorkStartWire {
            work_item: WorkStartItemWire::New(NewWorkItemWire {
                source_kind: WorkItemSourceKindWire::Issue,
                source_ref: Some("#55".to_owned()),
                title: None,
                goal: "dogfood".to_owned(),
            }),
            head: None,
            git: GitObservationWire::Unknown,
            owner_agent: None,
        })
        .expect("json");
        let output = self.work("start", &input, true);
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        match serde_json::from_slice(&output.stdout).expect("start JSON") {
            WorkResponse::Started(started) => started.working_state.work_item,
            other => panic!("expected Started, got {other:?}"),
        }
    }

    /// `work result --json`: the exit code and the response.
    fn result(&self, input: &WorkResultInputWire) -> (Option<i32>, WorkResponse, String) {
        let output = self.work("result", &serde_json::to_string(input).expect("json"), true);
        let response = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("result JSON: {}", text(&output.stderr)));
        (output.status.code(), response, text(&output.stderr))
    }

    /// `find files --json`: every path and whether the answer was current.
    fn files(&self) -> (Vec<String>, bool) {
        let output = self.on(&["find", "files", "--recursive"], true, None);
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        let response: QueryResponse = serde_json::from_slice(&output.stdout).expect("find JSON");
        let QueryOutcomeWire::Ok(QueryResultWire::Find(FindResultWire::Files(listing))) =
            response.outcome
        else {
            panic!("expected a file listing")
        };
        (
            listing
                .entries
                .into_iter()
                .map(|entry| entry.path_rel)
                .collect(),
            listing.currentness == CurrentnessWire::Current,
        )
    }

    /// `find target --symbol-name <name> --json` as text.
    fn symbol(&self, name: &str) -> (Option<i32>, String) {
        let output = self.on(
            &[
                "find",
                "target",
                "--symbol-name",
                name,
                "--budget",
                "compact",
                "--retention",
                "disabled",
            ],
            true,
            None,
        );
        (output.status.code(), text(&output.stdout))
    }
}

struct DaemonGuard(Child);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The real daemon, spawned, with one initialized crate Workspace.
struct Spawned {
    _daemon: DaemonGuard,
    home: TestDir,
    root: TestDir,
    cli: Cli,
}

impl Spawned {
    fn new(label: &str) -> Self {
        let home = TestDir::create(&format!("{label}-home"));
        let root = TestDir::create(&format!("{label}-root"));
        crate_tree(root.path());
        let daemon = DaemonGuard(
            Command::new(env!("CARGO_BIN_EXE_brainprintd"))
                .env("HOME", home.path())
                .env("USERPROFILE", home.path())
                .env_remove("XDG_RUNTIME_DIR")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("brainprintd"),
        );
        let cli = Cli {
            home: home.path().to_path_buf(),
            workspace: root.path().to_string_lossy().into_owned(),
        };
        wait_until("the daemon", || cli.run(&["status"], None).status.success());
        let init = cli.run(&["init", &cli.workspace], None);
        assert!(init.status.success(), "{}", text(&init.stderr));
        Self {
            _daemon: daemon,
            home,
            root,
            cli,
        }
    }
}

fn result_input(work_item: WorkItemId) -> WorkResultInputWire {
    WorkResultInputWire {
        work_item,
        outcome: WorkOutcomeWire::Partial,
        summary: "done".to_owned(),
        commit_id: None,
        verification_summary: None,
        verification: None,
        verification_job: None,
        git: GitObservationWire::Unknown,
        change_set: None,
    }
}

fn attach(work_item: WorkItemId, job: VerificationJobId) -> WorkResultInputWire {
    WorkResultInputWire {
        verification_job: Some(job),
        ..result_input(work_item)
    }
}

fn finished_parts(
    poll: &VerificationJobPollWire,
) -> (&str, &Vec<CommandResultWire>, &PostCommandRefreshWire) {
    match &poll.events.last().expect("events").payload {
        VerificationJobEventPayloadWire::JobFinished {
            verification_summary,
            results,
            refresh: Some(refresh),
        } => (verification_summary, results, refresh),
        other => panic!("expected a v2 JobFinished, got {other:?}"),
    }
}

/// `(before, after, created, updated, deleted, total, changes, omitted)`.
type Current<'a> = (
    &'a IndexBasisWire,
    &'a IndexBasisWire,
    u64,
    u64,
    u64,
    u64,
    &'a Vec<ResourceDeltaWire>,
    u64,
);

fn current(refresh: &PostCommandRefreshWire) -> Current<'_> {
    let PostCommandRefreshWire::Current {
        before,
        after,
        created_count,
        updated_count,
        deleted_count,
        total_changed,
        changes,
        delivery_omitted,
    } = refresh
    else {
        panic!("expected a Current refresh, got {refresh:?}")
    };
    (
        before,
        after,
        *created_count,
        *updated_count,
        *deleted_count,
        *total_changed,
        changes,
        *delivery_omitted,
    )
}

fn revision(basis: &IndexBasisWire) -> u64 {
    basis.workspace_revision.parse().expect("numeric revision")
}

fn recorded_of(response: WorkResponse) -> WorkRecordedWire {
    match response {
        WorkResponse::Recorded(recorded) => recorded,
        other => panic!("expected Recorded, got {other:?}"),
    }
}

fn failed(response: WorkResponse) -> WorkFailureWire {
    match response {
        WorkResponse::Failed(failure) => failure,
        other => panic!("expected Failed, got {other:?}"),
    }
}

fn result_rows(root: &Path) -> (i64, i64) {
    Connection::open_with_flags(
        WorkspacePaths::from_root(root).workspace_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("workspace.db")
    .query_row(
        "SELECT COUNT(*), COUNT(verification_job_id) FROM work_result",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .expect("count")
}

// ------------------------------------------------------------- dogfood

/// The whole loop through the shipped binaries: `cargo fmt` as a managed
/// Job; the terminal poll says what changed and where the index is; the
/// very next query is current and sees the formatted structure; the Job
/// is the Work Result's verification without running again; a Job the
/// Workspace has moved past is refused; a synchronous verification
/// refreshes before it records.
fn dogfood_formatter_refresh_query_and_attach() {
    let spawned = Spawned::new("dogfood");
    let cli = &spawned.cli;
    let root = spawned.root.path();
    let runs = spawned.home.path().join("runs");
    let first = cli.started();
    let second = cli.started();

    let (job, returned) = cli.start(
        "fmt-1",
        &batch(vec![
            VerificationCommandWire {
                argv: vec!["cargo".to_owned(), "fmt".to_owned(), "--all".to_owned()],
                ..command("fmt", &[])
            },
            command("count", &[&op("mark", &runs)]),
        ]),
    );
    let poll = cli.terminal(job);
    assert_eq!(poll.state, VerificationJobStateWire::Finished);
    let (summary, results, refresh) = finished_parts(&poll);
    assert!(
        results
            .iter()
            .all(|result| result.outcome == VerificationOutcomeWire::Passed),
        "{results:?}"
    );
    let (before, after, created, updated, deleted, total, changes, omitted) = current(refresh);
    assert!(updated >= 1, "{refresh:?}");
    assert!(
        changes
            .iter()
            .any(|change| change.path == "src/lib.rs"
                && change.kind == ResourceDeltaKindWire::Updated)
    );
    assert!(revision(after) > revision(before));
    assert!(after.generation_no > before.generation_no);
    assert_eq!(total, created + updated + deleted);
    assert_eq!(omitted, 0);
    assert_ne!(
        fs::read_to_string(root.join("src/lib.rs")).expect("lib"),
        LIB_RS
    );
    println!(
        "dogfood: start returned {}ms running; job {job}; before rev {} gen {}; after rev {} gen {}; created {created} updated {updated} deleted {deleted}",
        returned.as_millis(),
        before.workspace_revision,
        before.generation_no,
        after.workspace_revision,
        after.generation_no
    );

    // The compact terminal: one Workspace line, no path list.
    let compact = cli.compact_poll(job);
    let line = format!(
        "    workspace current revision {} generation {}; changed {total} (created {created}, updated {updated}, deleted {deleted})",
        after.workspace_revision, after.generation_no
    );
    assert!(compact.lines().any(|text| text == line), "{compact}");
    assert!(!compact.contains("src/lib.rs"), "{compact}");

    // Immediately: current, and the formatter's structure is the index's.
    let (files, is_current) = cli.files();
    assert!(is_current && files.iter().any(|path| path == "src/lib.rs"));
    let (code, sub) = cli.symbol("sub");
    assert_eq!(code, Some(0), "{sub}");
    assert!(sub.contains("\"currentness\":\"Current\""), "{sub}");
    // rustfmt's layout, not the original one-line form.
    assert!(
        sub.contains("\"signature\":\"pub fn sub(a: i32, b: i32) -> i32\"")
            && sub.contains("\"start\":{\"line\":6,"),
        "{sub}"
    );
    println!("dogfood: post-terminal find files current={is_current}; find target sub exit 0");

    // The Job is the result's verification: nothing runs again.
    let attached = cli.work(
        "result",
        &serde_json::to_string(&attach(first, job)).expect("json"),
        false,
    );
    assert_eq!(
        attached.status.code(),
        Some(0),
        "{}",
        text(&attached.stderr)
    );
    assert!(
        text(&attached.stdout).contains(&format!("  verification job {job}\n")),
        "{}",
        text(&attached.stdout)
    );
    let (code, response, _) = cli.result(&attach(first, job));
    assert_eq!(code, Some(0));
    let recorded = recorded_of(response);
    assert_eq!(recorded.result.verification_job, Some(job));
    assert_eq!(
        recorded.result.verification_summary.as_deref(),
        Some(summary)
    );
    assert_eq!(
        recorded.result.result_workspace_revision,
        after.workspace_revision
    );
    assert_eq!(
        recorded.result.result_generation_no,
        Some(after.generation_no)
    );
    assert_eq!(
        recorded.result.result_index_incarnation,
        Some(after.index_incarnation)
    );
    assert_eq!((recorded.verification, recorded.refresh), (None, None));
    assert_eq!(count(&runs), 1, "the Job ran once");
    println!(
        "dogfood: attached job {job} to {first}; runs {}",
        count(&runs)
    );

    // A query reads the provenance back.
    let resume = cli.on(
        &[
            "context",
            "resume",
            "--work-item",
            &first.to_string(),
            "--budget",
            "wide",
            "--retention",
            "disabled",
        ],
        true,
        None,
    );
    assert_eq!(resume.status.code(), Some(0), "{}", text(&resume.stderr));
    assert!(
        text(&resume.stdout).contains(&format!("\"verification_job\":\"{job}\"")),
        "{}",
        text(&resume.stdout)
    );

    // A synchronous verification refreshes before it records, at `after`.
    let mut sync = result_input(second);
    sync.verification = Some(batch(vec![command(
        "gen",
        &[&op("write", &root.join("src/generated.rs"))],
    )]));
    let (code, response, _) = cli.result(&sync);
    assert_eq!(code, Some(0));
    let recorded = recorded_of(response);
    let refresh = recorded.refresh.as_ref().expect("a sync refresh");
    let (_, sync_after, created, ..) = current(refresh);
    assert_eq!(created, 1);
    assert_eq!(
        recorded.result.result_workspace_revision,
        sync_after.workspace_revision
    );
    assert_eq!(
        recorded.result.result_generation_no,
        Some(sync_after.generation_no)
    );
    assert_eq!(recorded.result.verification_job, None);
    let (files, is_current) = cli.files();
    assert!(is_current && files.iter().any(|path| path == "src/generated.rs"));

    // The Workspace moved past the formatter Job: refused, nothing stored
    // or run.
    let before_rows = result_rows(root);
    let (code, response, stderr) = cli.result(&attach(second, job));
    assert_eq!(code, Some(4), "{stderr}");
    assert_eq!(failed(response).error, WorkErrorWire::VerificationJobStale);
    assert!(stderr.contains("stale"), "{stderr}");
    assert_eq!(result_rows(root), before_rows);
    assert_eq!(count(&runs), 1);
    println!("dogfood: stale attach exit 4; runs {}", count(&runs));

    // A caller's summary replaces the attached result, provenance and all.
    let mut by_hand = result_input(first);
    by_hand.verification_summary = Some("by hand".to_owned());
    let (code, response, _) = cli.result(&by_hand);
    assert_eq!(code, Some(0));
    assert_eq!(recorded_job(response), None);
    assert_eq!(result_rows(root), (2, 0));
}

fn recorded_job(response: WorkResponse) -> Option<VerificationJobId> {
    recorded_of(response).result.verification_job
}

/// Same length, other bytes, mtime put back: the Job's terminal poll
/// still reports the update, and the next query has the new symbol.
fn dogfood_same_size_same_mtime_through_poll() {
    let spawned = Spawned::new("same-size");
    let cli = &spawned.cli;
    let lib = spawned.root.path().join("src/lib.rs");
    let (job, _) = cli.start(
        "swap-1",
        &batch(vec![command("swap", &[&op("swap", &lib)])]),
    );
    let poll = cli.terminal(job);
    let (_, _, refresh) = finished_parts(&poll);
    let (before, after, created, updated, deleted, _, changes, _) = current(refresh);
    assert_eq!((created, updated, deleted), (0, 1, 0), "{refresh:?}");
    assert_eq!(changes.len(), 1);
    assert_eq!(
        (changes[0].kind, changes[0].path.as_str()),
        (ResourceDeltaKindWire::Updated, "src/lib.rs")
    );
    assert!(revision(after) > revision(before));
    let (code, zelper) = cli.symbol("zelper");
    assert_eq!(code, Some(0), "{zelper}");
    assert!(
        zelper.contains("\"currentness\":\"Current\"") && zelper.contains("\"name\":\"zelper\""),
        "{zelper}"
    );
    println!(
        "same-size: job {job}; rev {} -> {}; updated {updated}",
        before.workspace_revision, after.workspace_revision
    );
}

// --------------------------------------------------------- in-process

/// What the in-process daemon watches with.
enum Watch {
    /// Delivers nothing unless a test pushes it.
    Silent,
    /// No watcher: each settle calls the factory, which runs the armed
    /// action once its skip count is used up.
    Hook(Hook),
}

type Action = Box<dyn FnOnce() + Send>;

#[derive(Clone, Default)]
struct Hook(Arc<Mutex<Option<(usize, Action)>>>);

impl Hook {
    /// Run `action` in the settle after the next `skip` ones.
    fn arm(&self, skip: usize, action: impl FnOnce() + Send + 'static) {
        *self.0.lock().expect("hook") = Some((skip, Box::new(action)));
    }

    fn call(&self) {
        let mut armed = self.0.lock().expect("hook");
        match armed.take() {
            Some((0, action)) => action(),
            Some((skip, action)) => *armed = Some((skip - 1, action)),
            None => {}
        }
    }
}

struct Silent;

impl WatchSource for Silent {
    fn drain(&mut self) -> Vec<RawWatchEvent> {
        Vec::new()
    }
}

fn factory(watch: &Watch) -> WatchFactory {
    match watch {
        Watch::Silent => Arc::new(|_root: &Path| Ok(Box::new(Silent) as Box<dyn WatchSource>)),
        Watch::Hook(hook) => {
            let hook = hook.clone();
            Arc::new(move |_root: &Path| {
                hook.call();
                Err("no watcher (test)".to_owned())
            })
        }
    }
}

struct Ws {
    id: WorkspaceId,
    root: PathBuf,
    paths: WorkspacePaths,
}

struct Fixture {
    home: TestDir,
    roots: Vec<TestDir>,
    endpoint: runtime_paths::RuntimeEndpoint,
    runtime: Arc<DaemonQueryRuntime>,
    connection: ClientConnection,
    ws: Ws,
    cli: Cli,
    _server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new(label: &str, watch: &Watch) -> Self {
        let home = TestDir::create(&format!("{label}-home"));
        let global = GlobalPaths::from_home(home.path());
        let mut server = Server::bind_with_watch_factory(&global, factory(watch))
            .await
            .expect("bind");
        let runtime = server.query_runtime();
        let endpoint = runtime_paths::resolve(&global);
        let server = tokio::spawn(async move {
            let _ = server.serve().await;
        });
        let mut connection = connect(&endpoint, PROTOCOL_VERSION).await;
        let mut roots = Vec::new();
        let ws = workspace(&mut connection, &mut roots, label).await;
        let cli = Cli {
            home: home.path().to_path_buf(),
            workspace: ws.root.to_string_lossy().into_owned(),
        };
        Self {
            home,
            roots,
            endpoint,
            runtime,
            connection,
            ws,
            cli,
            _server: server,
        }
    }

    fn marker(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    async fn send(&mut self, request: &Request) -> Response {
        send(&mut self.connection, request).await
    }

    async fn work(&mut self, input: WorkResultInputWire) -> WorkResponse {
        let request = Request::Work(WorkRequest {
            workspace: selector(&self.ws),
            operation: WorkOperationWire::Result(input),
        });
        match self.send(&request).await {
            Response::Work(response) => response,
            other => panic!("expected Work, got {other:?}"),
        }
    }

    async fn started(&mut self) -> WorkItemId {
        let request = Request::Work(WorkRequest {
            workspace: selector(&self.ws),
            operation: WorkOperationWire::Start(WorkStartWire {
                work_item: WorkStartItemWire::New(NewWorkItemWire {
                    source_kind: WorkItemSourceKindWire::Issue,
                    source_ref: None,
                    title: None,
                    goal: "wire".to_owned(),
                }),
                head: None,
                git: GitObservationWire::Unknown,
                owner_agent: None,
            }),
        });
        match self.send(&request).await {
            Response::Work(WorkResponse::Started(started)) => started.working_state.work_item,
            other => panic!("expected Started, got {other:?}"),
        }
    }

    async fn start(
        &mut self,
        ws: Option<&Ws>,
        key: &str,
        verification: VerificationWire,
    ) -> VerificationJobId {
        let workspace = selector(ws.unwrap_or(&self.ws));
        let request = Request::VerificationJobStart(VerificationJobStartRequestWire {
            workspace,
            idempotency_key: key.to_owned(),
            verification,
        });
        match self.send(&request).await {
            Response::VerificationJobStart(Ok(started)) => started.job_id,
            other => panic!("expected a start, got {other:?}"),
        }
    }

    async fn poll_wire(&mut self, ws: Option<&Ws>, job: VerificationJobId) -> (Response, usize) {
        let workspace = selector(ws.unwrap_or(&self.ws));
        let request = Request::VerificationJobPoll(VerificationJobPollRequestWire {
            workspace,
            job_id: job,
            after_seq: 0,
            limit: MAX_POLL_EVENTS,
        });
        protocol::framing::write_message(&mut self.connection, &request)
            .await
            .expect("write");
        let response: Response = protocol::framing::read_message(&mut self.connection)
            .await
            .expect("read");
        let bytes = serde_json::to_vec(&response).expect("encode").len();
        (response, bytes)
    }

    async fn terminal(
        &mut self,
        ws: Option<&Ws>,
        job: VerificationJobId,
    ) -> VerificationJobPollWire {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let (response, _) = self.poll_wire(ws, job).await;
            let Response::VerificationJobPoll(Ok(poll)) = response else {
                panic!("poll failed: {response:?}")
            };
            if poll.state != VerificationJobStateWire::Running && !poll.has_more {
                return poll;
            }
            assert!(Instant::now() < deadline, "the Job never ended");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn reconciles(&self) -> u64 {
        self.runtime
            .lifecycle_stats(self.ws.id)
            .await
            .expect("runtime")
            .reconciles
    }

    async fn other(&mut self, label: &str) -> Ws {
        workspace(&mut self.connection, &mut self.roots, label).await
    }

    /// Runs the CLI off the async runtime.
    async fn cli(&self, args: Vec<String>, json: bool) -> Output {
        let cli = self.cli.clone();
        tokio::task::spawn_blocking(move || {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            cli.on(&args, json, None)
        })
        .await
        .expect("cli")
    }

    async fn cli_attach(&self, input: WorkResultInputWire) -> (Option<i32>, String) {
        let cli = self.cli.clone();
        tokio::task::spawn_blocking(move || {
            let output = cli.work(
                "result",
                &serde_json::to_string(&input).expect("json"),
                false,
            );
            (output.status.code(), text(&output.stderr))
        })
        .await
        .expect("cli")
    }
}

fn selector(ws: &Ws) -> WorkspaceSelectorWire {
    WorkspaceSelectorWire::Id {
        workspace_id: ws.id,
    }
}

async fn connect(endpoint: &runtime_paths::RuntimeEndpoint, version: u32) -> ClientConnection {
    let mut connection = raw_connect(endpoint).await;
    let response = send(
        &mut connection,
        &Request::Handshake(HandshakeRequest {
            protocol_version: version,
            client_kind: "i6-task6-step3".to_owned(),
        }),
    )
    .await;
    assert!(
        matches!(response, Response::Handshake(HandshakeResponse::Ok { .. })),
        "{response:?}"
    );
    connection
}

async fn raw_connect(endpoint: &runtime_paths::RuntimeEndpoint) -> ClientConnection {
    #[cfg(unix)]
    let connection = ClientConnection::connect(&endpoint.socket_path).await;
    #[cfg(windows)]
    let connection = ClientConnection::connect(&endpoint.pipe_name).await;
    connection.expect("connect")
}

async fn workspace(connection: &mut ClientConnection, roots: &mut Vec<TestDir>, label: &str) -> Ws {
    let root = TestDir::create(&format!("{label}-ws"));
    fs::create_dir_all(root.path().join("src")).expect("src");
    fs::write(
        root.path().join("src/lib.rs"),
        "pub fn helper() -> i32 {\n    42\n}\n",
    )
    .expect("lib.rs");
    let path = root.path().to_path_buf();
    roots.push(root);
    let response = send(
        connection,
        &Request::Init(InitRequest {
            path: path.to_string_lossy().into_owned(),
        }),
    )
    .await;
    let Response::Init(InitResponse { workspace_id, .. }) = response else {
        panic!("init failed: {response:?}")
    };
    Ws {
        id: workspace_id.parse().expect("id"),
        paths: WorkspacePaths::from_root(&path),
        root: path,
    }
}

async fn send(connection: &mut ClientConnection, request: &Request) -> Response {
    protocol::framing::write_message(connection, request)
        .await
        .expect("write");
    protocol::framing::read_message(connection)
        .await
        .expect("read")
}

/// The index's current basis, as stored.
fn stored_basis(ws: &Ws) -> IndexBasisWire {
    let db = Connection::open_with_flags(&ws.paths.index_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("index.db");
    let (workspace_revision, generation_no, generation_basis_revision) = db
        .query_row(
            "SELECT c.current_workspace_revision, g.generation_no, g.basis_workspace_revision \
             FROM workspace_clock c JOIN generation g ON g.id = c.stable_generation_id",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("clock");
    let incarnation: Vec<u8> = db
        .query_row(
            "SELECT index_incarnation_uid FROM db_meta WHERE id = 0",
            [],
            |row| row.get(0),
        )
        .expect("incarnation");
    IndexBasisWire {
        index_incarnation: IndexIncarnationId::from_bytes(incarnation.try_into().expect("16")),
        workspace_revision,
        generation_no,
        generation_basis_revision,
    }
}

fn terminal_payload(poll: &VerificationJobPollWire) -> &VerificationJobEventPayloadWire {
    &poll.events.last().expect("events").payload
}

fn compact_lines(output: &Output) -> Vec<String> {
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    text(&output.stdout).lines().map(str::to_owned).collect()
}

fn poll_args(job: VerificationJobId) -> Vec<String> {
    vec!["verification".into(), "poll".into(), job.to_string()]
}

// ----------------------------------------------------- managed wire

/// Current, NotNeeded and DeferredDaemonShutdown, as stored, on the wire
/// and in one compact line each.
fn managed_refresh_wire_and_compact_lines() {
    block_on(async {
        let mut fixture = Fixture::new("managed-wire", &Watch::Silent).await;
        let generated = fixture.ws.root.join("src/generated.rs");

        let job = fixture
            .start(
                None,
                "current",
                batch(vec![command("gen", &[&op("write", &generated)])]),
            )
            .await;
        let poll = fixture.terminal(None, job).await;
        let VerificationJobEventPayloadWire::JobFinished {
            refresh: Some(refresh),
            ..
        } = terminal_payload(&poll)
        else {
            panic!("{poll:?}")
        };
        let (_, after, created, updated, deleted, total, changes, omitted) = current(refresh);
        assert_eq!((created, updated, deleted, total, omitted), (1, 0, 0, 1, 0));
        assert_eq!(changes[0].path, "src/generated.rs");
        assert_eq!(after, &stored_basis(&fixture.ws));
        let lines = compact_lines(&fixture.cli(poll_args(job), false).await);
        assert!(
            lines.contains(&format!(
                "    workspace current revision {} generation {}; changed 1 (created 1, updated 0, deleted 0)",
                after.workspace_revision, after.generation_no
            )),
            "{lines:?}"
        );
        // `--json` is the wire, change list included.
        let json = fixture.cli(poll_args(job), true).await;
        let via_cli: VerificationJobPollResponseWire =
            serde_json::from_slice(&json.stdout).expect("json");
        assert_eq!(via_cli.expect("poll"), poll);

        let basis = stored_basis(&fixture.ws);
        let reconciles = fixture.reconciles().await;
        let job = fixture.start(None, "nothing", batch(vec![missing()])).await;
        let poll = fixture.terminal(None, job).await;
        assert_eq!(
            terminal_payload(&poll),
            &VerificationJobEventPayloadWire::JobFinished {
                verification_summary: match terminal_payload(&poll) {
                    VerificationJobEventPayloadWire::JobFinished {
                        verification_summary,
                        ..
                    } => verification_summary.clone(),
                    _ => unreachable!(),
                },
                results: match terminal_payload(&poll) {
                    VerificationJobEventPayloadWire::JobFinished { results, .. } => results.clone(),
                    _ => unreachable!(),
                },
                refresh: Some(PostCommandRefreshWire::NotNeeded {
                    basis: basis.clone()
                }),
            }
        );
        assert_eq!(fixture.reconciles().await, reconciles);
        let lines = compact_lines(&fixture.cli(poll_args(job), false).await);
        assert!(
            lines.contains(&format!(
                "    workspace unchanged at revision {} generation {}; no command started",
                basis.workspace_revision, basis.generation_no
            )),
            "{lines:?}"
        );

        let started = fixture.marker("started");
        let job = fixture
            .start(
                None,
                "shutdown",
                batch(vec![command(
                    "slow",
                    &[&op("mark", &started), &format!("sleep:{LATE_MS}")],
                )]),
            )
            .await;
        wait_for(&started);
        fixture.runtime.managed_shutdown().await;
        let poll = fixture.terminal(None, job).await;
        assert!(
            matches!(
                terminal_payload(&poll),
                VerificationJobEventPayloadWire::JobInterrupted {
                    reason: JobEndReasonWire::DaemonShutdown,
                    refresh: Some(PostCommandRefreshWire::DeferredDaemonShutdown {
                        before: Some(_)
                    }),
                }
            ),
            "{poll:?}"
        );
        let lines = compact_lines(&fixture.cli(poll_args(job), false).await);
        let at = lines
            .iter()
            .position(|line| line.ends_with("job interrupted DaemonShutdown"))
            .expect("interrupted line");
        assert_eq!(
            lines[at + 1],
            "    workspace refresh deferred until next currentness barrier"
        );
    });
}

/// Far more changes than 256 KiB of delta: exact counts, the bounded
/// prefix, the omission, one frame -- on the wire, in `--json` and in
/// the compact line.
fn a_large_delta_keeps_exact_counts_within_the_bounds() {
    block_on(async {
        let mut fixture = Fixture::new("large", &Watch::Silent).await;
        let many = fixture.ws.root.join("many");
        let job = fixture
            .start(
                None,
                "many",
                batch(vec![command(
                    "many",
                    &[&format!("many:{}|3000", many.display())],
                )]),
            )
            .await;
        fixture.terminal(None, job).await;
        let (response, bytes) = fixture.poll_wire(None, job).await;
        assert!(bytes <= MAX_MESSAGE_BYTES as usize, "{bytes}");
        let Response::VerificationJobPoll(Ok(poll)) = response else {
            panic!("{response:?}")
        };
        assert!(!poll.has_more, "one page");
        let VerificationJobEventPayloadWire::JobFinished {
            refresh: Some(refresh),
            ..
        } = terminal_payload(&poll)
        else {
            panic!("{poll:?}")
        };
        let (_, after, created, updated, deleted, total, changes, omitted) = current(refresh);
        // The 3000 files and their directory.
        assert_eq!((created, updated, deleted, total), (3001, 0, 0, 3001));
        let change_bytes = serde_json::to_vec(changes).expect("encode").len();
        assert!(
            change_bytes <= MAX_REFRESH_DELTA_WIRE_BYTES,
            "{change_bytes}"
        );
        assert!(omitted > 0 && !changes.is_empty());
        assert_eq!(omitted, total - changes.len() as u64);
        let mut sorted = changes.clone();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(&sorted, changes, "the deterministic prefix");
        println!(
            "large: total {total}, delivered {}, omitted {omitted}, changes {change_bytes}B, frame {bytes}B",
            changes.len()
        );

        let json = fixture.cli(poll_args(job), true).await;
        let via_cli: VerificationJobPollResponseWire =
            serde_json::from_slice(&json.stdout).expect("json");
        assert_eq!(via_cli.expect("poll"), poll, "--json loses nothing");
        let lines = compact_lines(&fixture.cli(poll_args(job), false).await);
        assert!(
            lines.contains(&format!(
                "    workspace current revision {} generation {}; changed 3001 (created 3001, updated 0, deleted 0); {omitted} change items omitted from delivery",
                after.workspace_revision, after.generation_no
            )),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("many/")),
            "no path flood"
        );
    });
}

/// A refresh that proves nothing, and a baseline that cannot be taken,
/// keep their own typed categories. Unix only: Windows does not rename a
/// file the daemon holds open.
fn failed_refresh_and_baseline_reason_are_typed() {
    #[cfg(unix)]
    block_on(async {
        let mut fixture = Fixture::new("failed", &Watch::Silent).await;
        let index = fixture.ws.paths.index_db.clone();
        let away = index.with_extension("away");
        let job = fixture
            .start(
                None,
                "failed",
                batch(vec![command(
                    "move",
                    &[&format!("move:{}|{}", index.display(), away.display())],
                )]),
            )
            .await;
        let poll = fixture.terminal(None, job).await;
        assert_eq!(poll.state, VerificationJobStateWire::Finished);
        assert!(
            matches!(
                terminal_payload(&poll),
                VerificationJobEventPayloadWire::JobFinished {
                    refresh: Some(PostCommandRefreshWire::Failed {
                        before: Some(_),
                        reason: RefreshFailureWire::ReconcileFailed,
                    }),
                    ..
                }
            ),
            "{poll:?}"
        );
        let lines = compact_lines(&fixture.cli(poll_args(job), false).await);
        assert!(
            lines.contains(&"    workspace refresh failed: ReconcileFailed".to_owned()),
            "{lines:?}"
        );

        // Still missing: the next Job's baseline fails before any command.
        let never = fixture.marker("never");
        let job = fixture
            .start(
                None,
                "baseline",
                batch(vec![command("never", &[&op("mark", &never)])]),
            )
            .await;
        let poll = fixture.terminal(None, job).await;
        fs::rename(&away, &index).expect("restore index.db");
        assert_eq!(poll.state, VerificationJobStateWire::InternalError);
        assert_eq!(
            terminal_payload(&poll),
            &VerificationJobEventPayloadWire::JobInternalError {
                reason: JobEndReasonWire::BaselineCurrentness,
                refresh: None,
            }
        );
        assert!(!never.exists(), "nothing ran");
        let lines = compact_lines(&fixture.cli(poll_args(job), false).await);
        assert!(
            lines
                .iter()
                .any(|line| line.ends_with("job internal error BaselineCurrentness")),
            "{lines:?}"
        );
    });
}

fn engine_command(wire: &VerificationCommandWire) -> VerificationCommand {
    VerificationCommand {
        label: wire.label.clone(),
        argv: wire.argv.clone(),
        cwd: wire.cwd.clone(),
        env: wire.env.clone(),
        timeout_secs: wire.timeout_secs,
        capture: None,
    }
}

/// A row as an older daemon (or a broken writer) left it.
fn stored_job(ws: &Ws, key: &str, finished: &str) -> VerificationJobId {
    let job = VerificationJobId::generate();
    let mut store = VerificationJobStore::open_bound(ws.id, &ws.paths.workspace_db).expect("jobs");
    store
        .create(&NewVerificationJob {
            uid: job,
            idempotency_key: &IdempotencyKey::parse(key).expect("key"),
            request_fingerprint: request_fingerprint(&[engine_command(&command(key, &[]))]),
            command_count: 1,
            started_payload_json: r#"{"v":1}"#,
        })
        .expect("job");
    store
        .append(
            job,
            ProgressEvent::CommandStarted,
            &format!(r#"{{"v":1,"index":0,"label":"{key}"}}"#),
        )
        .expect("event");
    store
        .finish(
            job,
            TerminalState::Finished,
            Some("legacy: passed"),
            finished,
        )
        .expect("finish");
    job
}

/// A v1 Job polls with no refresh -- not invented, not corrupt -- and is
/// never attached; a v2 JOB_FINISHED without one is corrupt.
fn v1_jobs_poll_without_refresh_and_v2_without_one_is_corrupt() {
    block_on(async {
        let mut fixture = Fixture::new("v1", &Watch::Silent).await;
        let v1 = stored_job(
            &fixture.ws,
            "legacy",
            r#"{"v":1,"verification_summary":"legacy: passed","results":[]}"#,
        );
        let poll = fixture.terminal(None, v1).await;
        assert_eq!(
            terminal_payload(&poll),
            &VerificationJobEventPayloadWire::JobFinished {
                verification_summary: "legacy: passed".to_owned(),
                results: Vec::new(),
                refresh: None,
            }
        );
        let work_item = fixture.started().await;
        let failure = failed(fixture.work(attach(work_item, v1)).await);
        assert_eq!(failure.error, WorkErrorWire::VerificationJobNoRefreshBasis);
        let (code, stderr) = fixture.cli_attach(attach(work_item, v1)).await;
        assert_eq!(code, Some(4), "{stderr}");
        assert!(
            stderr.contains("predates post-command freshness"),
            "{stderr}"
        );

        let corrupt = stored_job(
            &fixture.ws,
            "corrupt",
            r#"{"v":2,"verification_summary":"legacy: passed","results":[]}"#,
        );
        let (response, _) = fixture.poll_wire(None, corrupt).await;
        assert_eq!(
            response,
            Response::VerificationJobPoll(Err(VerificationJobErrorWire::Corrupt))
        );
        let failure = failed(fixture.work(attach(work_item, corrupt)).await);
        assert_eq!(failure.error, WorkErrorWire::VerificationJobCorrupt);
        assert_eq!(result_rows(&fixture.ws.root), (0, 0));
        assert_eq!(fixture.runtime.verification_runs(), 0);
    });
}

// ------------------------------------------------------------ sync wire

/// A synchronous verification's refresh on the Work response: Current at
/// the stored result's basis; NotNeeded at the baseline with no reconcile.
fn sync_refresh_facts_over_the_wire() {
    block_on(async {
        let mut fixture = Fixture::new("sync", &Watch::Silent).await;
        let work_item = fixture.started().await;
        let mut input = result_input(work_item);
        input.verification = Some(batch(vec![command(
            "gen",
            &[&op("write", &fixture.ws.root.join("src/generated.rs"))],
        )]));
        let recorded = recorded_of(fixture.work(input).await);
        let refresh = recorded.refresh.as_ref().expect("refresh");
        let (before, after, created, ..) = current(refresh);
        assert_eq!(created, 1);
        assert!(revision(after) > revision(before));
        assert_eq!(after, &stored_basis(&fixture.ws));
        assert_eq!(
            recorded.result.result_workspace_revision,
            after.workspace_revision
        );
        assert_eq!(
            recorded.result.result_generation_no,
            Some(after.generation_no)
        );
        assert!(recorded.verification.is_some());

        let basis = stored_basis(&fixture.ws);
        let reconciles = fixture.reconciles().await;
        let mut input = result_input(work_item);
        input.verification = Some(batch(vec![missing(), command("after", &["exit:0"])]));
        let recorded = recorded_of(fixture.work(input).await);
        assert_eq!(
            recorded.refresh,
            Some(PostCommandRefreshWire::NotNeeded {
                basis: basis.clone()
            })
        );
        assert_eq!(
            recorded.result.result_workspace_revision,
            basis.workspace_revision
        );
        assert_eq!(fixture.reconciles().await, reconciles);
    });
}

/// After the refresh, before the write, the Workspace moves: the worker's
/// own check refuses with the typed error; the commands' facts stay.
fn sync_basis_race_is_typed() {
    block_on(async {
        let hook = Hook::default();
        let mut fixture = Fixture::new("sync-race", &Watch::Hook(hook.clone())).await;
        let work_item = fixture.started().await;
        let (started, go) = (fixture.marker("started"), fixture.marker("go"));
        let mut input = result_input(work_item);
        input.verification = Some(batch(vec![command(
            "gen",
            &[
                &op("write", &fixture.ws.root.join("src/generated.rs")),
                &op("mark", &started),
                &op("await", &go),
            ],
        )]));
        let raced = fixture.ws.root.join("src/raced.rs");
        let mut other = connect(&fixture.endpoint, PROTOCOL_VERSION).await;
        let request = Request::Work(WorkRequest {
            workspace: selector(&fixture.ws),
            operation: WorkOperationWire::Result(input),
        });
        let running = tokio::spawn(async move { send(&mut other, &request).await });
        wait_for(&started);
        // Skip the refresh's settle; the write's own settle sees this.
        hook.arm(1, move || {
            fs::write(&raced, "pub fn raced() {}\n").expect("race");
        });
        fs::write(&go, "").expect("go");
        let Response::Work(response) = running.await.expect("request") else {
            panic!("expected Work")
        };
        let failure = failed(response);
        assert_eq!(failure.error, WorkErrorWire::VerificationResultBasisChanged);
        assert!(failure.verification.is_some());
        assert!(matches!(
            failure.refresh,
            Some(PostCommandRefreshWire::Current { .. })
        ));
        assert_eq!(result_rows(&fixture.ws.root), (0, 0));
        assert!(
            fixture.ws.root.join("src/raced.rs").exists(),
            "the race ran"
        );
    });
}

// ----------------------------------------------------------- attach wire

/// Through an ordinary Work request: a FINISHED Job is recorded with its
/// summary and ID, running nothing; every other Job, another Workspace's
/// Job, or a second verification source is refused with nothing written.
fn attach_over_the_wire_records_or_refuses_without_running() {
    block_on(async {
        let mut fixture = Fixture::new("attach", &Watch::Silent).await;
        let work_item = fixture.started().await;
        let runs = fixture.marker("runs");
        let job = fixture
            .start(
                None,
                "attach",
                batch(vec![command("count", &[&op("mark", &runs)])]),
            )
            .await;
        let poll = fixture.terminal(None, job).await;
        let (summary, _, _) = finished_parts(&poll);
        let summary = summary.to_owned();
        let started = fixture.runtime.verification_runs();

        // Exclusive sources: refused before anything is read or run.
        for input in [
            WorkResultInputWire {
                verification_summary: Some("mine".to_owned()),
                ..attach(work_item, job)
            },
            WorkResultInputWire {
                verification: Some(batch(vec![command("never", &["exit:0"])])),
                ..attach(work_item, job)
            },
        ] {
            let failure = failed(fixture.work(input).await);
            assert!(
                matches!(failure.error, WorkErrorWire::InvalidObservation { .. }),
                "{failure:?}"
            );
        }

        let recorded = recorded_of(fixture.work(attach(work_item, job)).await);
        assert_eq!(recorded.result.verification_job, Some(job));
        assert_eq!(recorded.result.verification_summary, Some(summary));
        assert_eq!((recorded.verification, recorded.refresh), (None, None));
        assert_eq!(result_rows(&fixture.ws.root), (1, 1));

        // Non-terminal and ended Jobs.
        let slow = fixture.marker("slow");
        let running = fixture
            .start(
                None,
                "running",
                batch(vec![command(
                    "slow",
                    &[&op("mark", &slow), &format!("sleep:{LATE_MS}")],
                )]),
            )
            .await;
        wait_for(&slow);
        let failure = failed(fixture.work(attach(work_item, running)).await);
        assert_eq!(
            failure.error,
            WorkErrorWire::VerificationJobNotFinished {
                state: VerificationJobStateWire::Running
            }
        );
        let cancel = Request::VerificationJobCancel(VerificationJobCancelRequestWire {
            workspace: selector(&fixture.ws),
            job_id: running,
        });
        fixture.send(&cancel).await;
        let failure = failed(fixture.work(attach(work_item, running)).await);
        assert_eq!(
            failure.error,
            WorkErrorWire::VerificationJobNotFinished {
                state: VerificationJobStateWire::Cancelled
            }
        );
        let (code, stderr) = fixture.cli_attach(attach(work_item, running)).await;
        assert_eq!(code, Some(4), "{stderr}");
        assert!(stderr.contains("is not FINISHED (Cancelled)"), "{stderr}");

        // Another Workspace's Job is not found here.
        let other = fixture.other("attach-other").await;
        let foreign = fixture
            .start(
                Some(&other),
                "foreign",
                batch(vec![command("x", &["exit:0"])]),
            )
            .await;
        fixture.terminal(Some(&other), foreign).await;
        let failure = failed(fixture.work(attach(work_item, foreign)).await);
        assert_eq!(failure.error, WorkErrorWire::VerificationJobNotFound);
        let failure = failed(
            fixture
                .work(attach(work_item, VerificationJobId::generate()))
                .await,
        );
        assert_eq!(failure.error, WorkErrorWire::VerificationJobNotFound);

        assert_eq!(result_rows(&fixture.ws.root), (1, 1));
        assert_eq!(count(&runs), 1, "the attached Job never ran again");
        assert_eq!(
            fixture.runtime.verification_runs(),
            started + 2,
            "only the two new Jobs ran"
        );

        // A synchronous result replaces the attached one, provenance too.
        let mut sync = result_input(work_item);
        sync.verification = Some(batch(vec![command("sync", &["exit:0"])]));
        let recorded = recorded_of(fixture.work(sync).await);
        assert_eq!(recorded.result.verification_job, None);
        assert_eq!(result_rows(&fixture.ws.root), (1, 0));
    });
}

/// The Job is read, then the Workspace moves before the write: the
/// worker refuses, typed, and no internal text crosses the wire.
fn attach_toctou_over_the_wire_is_stale() {
    block_on(async {
        let hook = Hook::default();
        let mut fixture = Fixture::new("toctou", &Watch::Hook(hook.clone())).await;
        let work_item = fixture.started().await;
        let job = fixture
            .start(None, "toctou", batch(vec![command("x", &["exit:0"])]))
            .await;
        fixture.terminal(None, job).await;
        let raced = fixture.ws.root.join("src/raced.rs");
        hook.arm(0, move || {
            fs::write(&raced, "pub fn raced() {}\n").expect("race");
        });
        let request = Request::Work(WorkRequest {
            workspace: selector(&fixture.ws),
            operation: WorkOperationWire::Result(attach(work_item, job)),
        });
        let response = fixture.send(&request).await;
        let json = serde_json::to_string(&response).expect("json");
        let Response::Work(response) = response else {
            panic!("expected Work")
        };
        assert_eq!(failed(response).error, WorkErrorWire::VerificationJobStale);
        assert!(
            fixture.ws.root.join("src/raced.rs").exists(),
            "the race ran"
        );
        assert!(!json.contains("Internal"), "{json}");
        assert_eq!(result_rows(&fixture.ws.root), (0, 0));
    });
}

// --------------------------------------------------------------- protocol

/// 9 ↔ 9 is served; an 8 client is refused by this daemon before any
/// request; the real CLI stops at an 8 daemon.
fn protocol_14_handshakes_strictly() {
    block_on(async {
        let mut fixture = Fixture::new("handshake", &Watch::Silent).await;
        let work_item = fixture.started().await;
        let mut old = raw_connect(&fixture.endpoint).await;
        assert_eq!(
            send(
                &mut old,
                &Request::Handshake(HandshakeRequest {
                    protocol_version: 13,
                    client_kind: "v13".to_owned(),
                })
            )
            .await,
            Response::Handshake(HandshakeResponse::VersionMismatch {
                server_protocol_version: 14,
                client_protocol_version: 13,
            })
        );
        let request = Request::Work(WorkRequest {
            workspace: selector(&fixture.ws),
            operation: WorkOperationWire::Result(result_input(work_item)),
        });
        let _ = protocol::framing::write_message(&mut old, &request).await;
        let reply: std::io::Result<Response> = protocol::framing::read_message(&mut old).await;
        assert!(reply.is_err(), "a v13 client is never served");
        assert_eq!(result_rows(&fixture.ws.root), (0, 0));
        let mut current = connect(&fixture.endpoint, 14).await;
        assert!(matches!(
            send(&mut current, &request).await,
            Response::Work(WorkResponse::Recorded(_))
        ));
    });

    block_on(async {
        let home = TestDir::create("v13-daemon");
        let endpoint = runtime_paths::resolve(&GlobalPaths::from_home(home.path()));
        #[cfg(unix)]
        let listener = {
            fs::create_dir_all(endpoint.socket_path.parent().expect("parent")).expect("dir");
            protocol::Listener::bind(&endpoint.socket_path).expect("listener")
        };
        #[cfg(windows)]
        let listener = protocol::Listener::bind(&endpoint.pipe_name).expect("listener");
        let fake_v13 = tokio::spawn(async move {
            let mut listener = listener;
            let mut connection = listener.accept().await.expect("accept");
            let Request::Handshake(handshake) = protocol::framing::read_message(&mut connection)
                .await
                .expect("handshake")
            else {
                panic!("the handshake first")
            };
            protocol::framing::write_message(
                &mut connection,
                &Response::Handshake(HandshakeResponse::VersionMismatch {
                    server_protocol_version: 13,
                    client_protocol_version: handshake.protocol_version,
                }),
            )
            .await
            .expect("reply");
            handshake.protocol_version
        });
        let cli = Cli {
            home: home.path().to_path_buf(),
            workspace: home.path().to_string_lossy().into_owned(),
        };
        let input = serde_json::to_string(&attach(
            WorkItemId::generate(),
            VerificationJobId::generate(),
        ))
        .expect("json");
        let output = tokio::task::spawn_blocking(move || cli.work("result", &input, false))
            .await
            .expect("cli");
        assert_eq!(output.status.code(), Some(5), "{}", text(&output.stderr));
        assert!(text(&output.stderr).contains("protocol version mismatch"));
        // #66: the stale side and the remedy are named.
        assert!(
            text(&output.stderr).contains("the running brainprintd is older"),
            "{}",
            text(&output.stderr)
        );
        assert_eq!(fake_v13.await.expect("fake daemon"), 14);
    });
}

fn protocol_is_14_schema_7_payload_2() {
    assert_eq!(PROTOCOL_VERSION, 14);
    assert_eq!(
        brainprint_engine::schema::workspace::WORKSPACE_MIGRATIONS.len(),
        7
    );
    assert_eq!(
        brainprint_daemon::query::MANAGED_JOB_EVENT_PAYLOAD_VERSION,
        2
    );
    assert_eq!(MAX_REFRESH_DELTA_WIRE_BYTES, 256 * 1024);
    // The new input field is optional on the wire; unknown fields are not.
    let minimal = r#"{"work_item":"00000000-0000-4000-8000-000000000001","outcome":"Partial","summary":"s","commit_id":null,"verification_summary":null,"verification":null,"git":"Unknown","change_set":null}"#;
    let parsed: WorkResultInputWire = serde_json::from_str(minimal).expect("no verification_job");
    assert_eq!(parsed.verification_job, None);
    let unknown = minimal.replace("\"summary\"", "\"bogus\":1,\"summary\"");
    assert!(serde_json::from_str::<WorkResultInputWire>(&unknown).is_err());
    let _ = NonZeroUsize::MIN;
}
