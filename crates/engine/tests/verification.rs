//! #52 step 2: verification batches, driven by this test binary run as a
//! portable helper (no shell), so the same tests hold on macOS, Ubuntu
//! and Windows. `harness = false` for the same reason as `process_runner`.
//!
//! Helper ops, run in order: `exit:<code>`, `sleep:<ms>`, `out:<bytes>`,
//! `err:<bytes>`, `mark:<path>`, `abort`, `spawn [ ops… ]` (a child left
//! running), `dump:<path>` (write argv after it, cwd and env as JSON to
//! `<path>` and stop: the remaining argv is payload, not ops).

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use brainprint_engine::{
    process_runner::Cancel,
    verification::{
        Cancelled, CommandResult, NotStartedReason, VerificationCommand, VerificationOutcome,
        VerificationReport, prepare,
    },
};

const HELPER: &str = "--brainprint-verification-helper";
/// Longer than any deadline: a helper still running would write it late.
const LATE_MS: u64 = 4000;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(HELPER) {
        helper(&args[1..]);
        return;
    }
    let tests: &[(&str, fn())] = &[
        ("invalid_batches_run_nothing", invalid_batches_run_nothing),
        (
            "cwd_must_stay_inside_the_root",
            cwd_must_stay_inside_the_root,
        ),
        (
            "outcomes_and_skip_after_first_non_pass",
            outcomes_and_skip_after_first_non_pass,
        ),
        ("timeout_is_timed_out", timeout_is_timed_out),
        ("unix_signal_is_signaled", unix_signal_is_signaled),
        ("argv_is_passed_literally", argv_is_passed_literally),
        (
            "env_is_inherited_with_caller_overrides",
            env_is_inherited_with_caller_overrides,
        ),
        ("runs_in_the_resolved_cwd", runs_in_the_resolved_cwd),
        (
            "exact_byte_counts_and_large_output",
            exact_byte_counts_and_large_output,
        ),
        (
            "cancel_ends_the_tree_and_the_batch",
            cancel_ends_the_tree_and_the_batch,
        ),
    ];
    let handles: Vec<_> = tests
        .iter()
        .map(|&(name, test)| (name, thread::spawn(test)))
        .collect();
    let mut failed = 0;
    for (name, handle) in handles {
        let ok = handle.join().is_ok();
        println!("test {name} ... {}", if ok { "ok" } else { "FAILED" });
        failed += usize::from(!ok);
    }
    println!("{} passed; {failed} failed", tests.len() - failed);
    if failed > 0 {
        std::process::exit(101);
    }
}

// ---------------------------------------------------------------- helper

#[allow(
    clippy::zombie_processes,
    reason = "descendants are left running on purpose; the runner must end them"
)]
fn helper(ops: &[String]) {
    let mut index = 0;
    while index < ops.len() {
        let op = ops[index].as_str();
        index += 1;
        if op == "spawn" {
            let end = closing(ops, index);
            Command::new(std::env::current_exe().expect("exe"))
                .arg(HELPER)
                .args(&ops[index + 1..end])
                .spawn()
                .expect("spawn helper");
            index = end + 1;
            continue;
        }
        if op == "abort" {
            std::process::abort();
        }
        let (name, value) = op.split_once(':').expect("op:value");
        match name {
            "exit" => std::process::exit(value.parse().expect("code")),
            "sleep" => thread::sleep(Duration::from_millis(value.parse().expect("ms"))),
            "out" => write_zeros(&mut std::io::stdout(), value),
            "err" => write_zeros(&mut std::io::stderr(), value),
            "mark" => fs::write(value, b"").expect("marker"),
            "dump" => {
                let env: BTreeMap<String, String> = std::env::vars_os()
                    .map(|(key, value)| {
                        (
                            key.to_string_lossy().into_owned(),
                            value.to_string_lossy().into_owned(),
                        )
                    })
                    .collect();
                let dump = serde_json::json!({
                    "args": &ops[index..],
                    "cwd": std::env::current_dir().expect("cwd"),
                    "env": env,
                });
                fs::write(value, dump.to_string()).expect("dump");
                return;
            }
            other => panic!("unknown helper op {other}"),
        }
    }
}

/// Index of the `]` matching the `[` at `open`.
fn closing(ops: &[String], open: usize) -> usize {
    assert_eq!(ops[open], "[");
    let mut depth = 0;
    for (index, op) in ops.iter().enumerate().skip(open) {
        match op.as_str() {
            "[" => depth += 1,
            "]" => {
                depth -= 1;
                if depth == 0 {
                    return index;
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced [ ]");
}

fn write_zeros(stream: &mut impl Write, count: &str) {
    let mut left: u64 = count.parse().expect("bytes");
    let chunk = [0_u8; 64 * 1024];
    while left > 0 {
        let now = left.min(chunk.len() as u64);
        stream.write_all(&chunk[..now as usize]).expect("write");
        left -= now;
    }
    stream.flush().expect("flush");
}

// --------------------------------------------------------------- harness

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "brainprint-verification-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }

    fn path(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn helper_argv(ops: &[&str]) -> Vec<String> {
    let exe = std::env::current_exe().expect("exe");
    [exe.to_string_lossy().as_ref(), HELPER]
        .into_iter()
        .chain(ops.iter().copied())
        .map(str::to_owned)
        .collect()
}

fn command(label: &str, ops: &[&str]) -> VerificationCommand {
    VerificationCommand {
        label: label.to_owned(),
        argv: helper_argv(ops),
        cwd: None,
        env: Vec::new(),
        timeout_secs: 60,
    }
}

fn run(root: &Path, commands: &[VerificationCommand]) -> VerificationReport {
    prepare(root, commands)
        .expect("valid batch")
        .run(&Cancel::default())
        .expect("not cancelled")
}

fn outcomes(report: &VerificationReport) -> Vec<VerificationOutcome> {
    report.results.iter().map(|result| result.outcome).collect()
}

fn dump(path: &str) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).expect("dump written")).expect("json")
}

// ----------------------------------------------------------------- tests

fn invalid_batches_run_nothing() {
    let dir = TestDir::create("invalid");
    let marker = dir.path("ran");
    let first = command("first", &[&format!("mark:{marker}")]);
    let with = |edit: fn(&mut VerificationCommand)| {
        let mut bad = command("second", &[]);
        edit(&mut bad);
        vec![first.clone(), bad]
    };
    let many = |count: usize| {
        (0..count)
            .map(|index| {
                let mut command = command(&format!("c{index}"), &[]);
                command.timeout_secs = 1;
                command
            })
            .collect::<Vec<_>>()
    };
    let cases: Vec<(&str, Vec<VerificationCommand>)> = vec![
        ("no commands", Vec::new()),
        ("17 commands", many(17)),
        ("duplicate label", with(|c| c.label = "first".to_owned())),
        ("empty label", with(|c| c.label = String::new())),
        ("bad label char", with(|c| c.label = "a b".to_owned())),
        ("65-char label", with(|c| c.label = "x".repeat(65))),
        ("empty argv", with(|c| c.argv.clear())),
        ("257 argv", with(|c| c.argv.resize(257, "x".to_owned()))),
        ("argv over 4 KiB", with(|c| c.argv.push("x".repeat(4097)))),
        ("empty argv[0]", with(|c| c.argv[0] = String::new())),
        ("NUL in argv", with(|c| c.argv.push("a\0b".to_owned()))),
        ("timeout 0", with(|c| c.timeout_secs = 0)),
        ("timeout 3601", with(|c| c.timeout_secs = 3601)),
        ("total over 3600", with(|c| c.timeout_secs = 3600 - 59)),
        (
            "33 env",
            with(|c| c.env = (0..33).map(|i| (format!("K{i}"), String::new())).collect()),
        ),
        (
            "bad env key",
            with(|c| c.env = vec![("1A".to_owned(), String::new())]),
        ),
        (
            "env value over 4 KiB",
            with(|c| c.env = vec![("K".to_owned(), "x".repeat(4097))]),
        ),
        ("absolute cwd", with(|c| c.cwd = Some(absolute()))),
        ("`..` cwd", with(|c| c.cwd = Some("..".to_owned()))),
        (
            "inner `..` cwd",
            with(|c| c.cwd = Some("a/../..".to_owned())),
        ),
        (
            "missing cwd",
            with(|c| c.cwd = Some("no-such-dir".to_owned())),
        ),
    ];
    for (name, commands) in cases {
        assert!(prepare(&dir.0, &commands).is_err(), "{name} was accepted");
    }
    assert!(!Path::new(&marker).exists(), "an invalid batch ran");

    // The bounds themselves are allowed.
    let mut edge = many(16);
    edge[0].timeout_secs = 3600 - 15;
    edge[1].argv.resize(256, "x".repeat(4096));
    edge[2].env = (0..32)
        .map(|i| (format!("_K{i}"), "x".repeat(4096)))
        .collect();
    edge[3].label = "x".repeat(64);
    assert!(prepare(&dir.0, &edge).is_ok());
}

fn absolute() -> String {
    std::env::temp_dir().to_string_lossy().into_owned()
}

fn cwd_must_stay_inside_the_root() {
    let dir = TestDir::create("cwd");
    let root = dir.0.join("root");
    fs::create_dir_all(root.join("sub")).expect("sub");
    fs::create_dir_all(dir.0.join("outside")).expect("outside");
    fs::write(root.join("file"), b"").expect("file");
    let at = |cwd: &str| {
        let mut command = command("c", &[]);
        command.cwd = Some(cwd.to_owned());
        prepare(&root, &[command])
    };
    assert!(at("sub").is_ok());
    assert!(at("file").is_err(), "a file is not a cwd");
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(dir.0.join("outside"), root.join("escape")).expect("symlink");
        symlink(root.join("sub"), root.join("inner")).expect("symlink");
        assert!(at("escape").is_err(), "symlink escape was accepted");
        assert!(at("inner").is_ok(), "a symlink inside the root is fine");
    }
}

fn outcomes_and_skip_after_first_non_pass() {
    let dir = TestDir::create("outcomes");
    let (skipped, skipped_too) = (dir.path("skipped"), dir.path("skipped-too"));
    let report = run(
        &dir.0,
        &[
            command("ok", &[]),
            command("fail", &["exit:3"]),
            command("later", &[&format!("mark:{skipped}")]),
            command("last", &[&format!("mark:{skipped_too}")]),
        ],
    );
    assert_eq!(
        outcomes(&report),
        [
            VerificationOutcome::Passed,
            VerificationOutcome::Failed { exit_code: 3 },
            VerificationOutcome::Skipped,
            VerificationOutcome::Skipped,
        ]
    );
    assert!(!Path::new(&skipped).exists() && !Path::new(&skipped_too).exists());
    for skipped in &report.results[2..] {
        assert_eq!(
            (
                skipped.duration_ms,
                skipped.stdout_bytes,
                skipped.stderr_bytes
            ),
            (0, 0, 0)
        );
    }
    let labels: Vec<_> = report.results.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["ok", "fail", "later", "last"]);
    let pattern = regex::Regex::new(
        r"^ok: passed \d+ms; fail: failed exit=3 \d+ms; later: skipped; last: skipped$",
    )
    .expect("regex");
    assert!(pattern.is_match(&report.summary), "{}", report.summary);

    let mut missing = command("gone", &[]);
    missing.argv = vec!["brainprint-no-such-program-52".to_owned()];
    let marker = dir.path("after-missing");
    let report = run(
        &dir.0,
        &[missing, command("after", &[&format!("mark:{marker}")])],
    );
    assert_eq!(
        outcomes(&report),
        [
            VerificationOutcome::NotStarted {
                reason: NotStartedReason::NotFound
            },
            VerificationOutcome::Skipped,
        ]
    );
    assert!(!Path::new(&marker).exists());
    assert!(
        report.summary.starts_with("gone: not-started not-found ")
            && report.summary.ends_with("ms; after: skipped"),
        "{}",
        report.summary
    );
}

fn timeout_is_timed_out() {
    let mut slow = command("slow", &["sleep:30000"]);
    slow.timeout_secs = 1;
    let dir = TestDir::create("timeout");
    let started = Instant::now();
    let report = run(&dir.0, &[slow]);
    assert_eq!(outcomes(&report), [VerificationOutcome::TimedOut]);
    assert!(report.results[0].duration_ms >= 1000);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(report.summary.starts_with("slow: timed-out "));
}

fn unix_signal_is_signaled() {
    if cfg!(unix) {
        let dir = TestDir::create("signal");
        let report = run(&dir.0, &[command("sig", &["abort"]), command("next", &[])]);
        // SIGABRT, raised by the command itself.
        assert_eq!(
            outcomes(&report),
            [
                VerificationOutcome::Signaled { signal: 6 },
                VerificationOutcome::Skipped
            ]
        );
        assert!(report.summary.starts_with("sig: signaled signal=6 "));
    }
}

fn argv_is_passed_literally() {
    let dir = TestDir::create("argv");
    let out = dir.path("dump");
    let payload = ["a;b", "$HOME", "*", "x y", "'q'", "|", "%PATH%"];
    let dump_op = format!("dump:{out}");
    let mut ops = vec![dump_op.as_str()];
    ops.extend(payload);
    let report = run(&dir.0, &[command("argv", &ops)]);
    assert_eq!(outcomes(&report), [VerificationOutcome::Passed]);
    assert_eq!(dump(&out)["args"], serde_json::json!(payload));
    assert!(!report.summary.contains("$HOME") && !report.summary.contains("a;b"));
}

fn env_is_inherited_with_caller_overrides() {
    let dir = TestDir::create("env");
    let out = dir.path("dump");
    let parent: BTreeMap<String, String> = std::env::vars_os()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    let (inherited, _) = parent
        .iter()
        .find(|(key, _)| key.as_str() != "PATH")
        .expect("some inherited variable");
    let mut probe = command("env", &[&format!("dump:{out}")]);
    probe.env = vec![
        ("BRAINPRINT_VERIFY_PROBE".to_owned(), "caller".to_owned()),
        (inherited.clone(), "overridden".to_owned()),
    ];
    let report = run(&dir.0, &[probe]);
    assert_eq!(outcomes(&report), [VerificationOutcome::Passed]);
    let env = dump(&out)["env"].clone();
    assert_eq!(env["BRAINPRINT_VERIFY_PROBE"], "caller");
    assert_eq!(env[inherited.as_str()], "overridden");
    let child = env.as_object().expect("env map");
    for (key, value) in &parent {
        if key != inherited {
            assert_eq!(child.get(key), Some(&serde_json::json!(value)), "{key}");
        }
    }
    let extra: Vec<_> = child
        .keys()
        .filter(|key| !parent.contains_key(*key) && *key != "BRAINPRINT_VERIFY_PROBE")
        .collect();
    assert!(extra.is_empty(), "variables added: {extra:?}");
}

fn runs_in_the_resolved_cwd() {
    let dir = TestDir::create("rundir");
    fs::create_dir_all(dir.0.join("a/b")).expect("dirs");
    let (at_root, at_sub) = (dir.path("root-dump"), dir.path("sub-dump"));
    let mut sub = command("sub", &[&format!("dump:{at_sub}")]);
    sub.cwd = Some("a/b".to_owned());
    run(
        &dir.0,
        &[command("root", &[&format!("dump:{at_root}")]), sub],
    );
    let cwd = |path: &str| {
        fs::canonicalize(dump(path)["cwd"].as_str().expect("cwd")).expect("canonical cwd")
    };
    let root = fs::canonicalize(&dir.0).expect("root");
    assert_eq!(cwd(&at_root), root);
    assert_eq!(cwd(&at_sub), root.join("a").join("b"));
}

fn exact_byte_counts_and_large_output() {
    const BIG: u64 = 64 * 1024 * 1024 + 12_345;
    let dir = TestDir::create("bytes");
    let report = run(
        &dir.0,
        &[
            command("small", &["out:123457", "err:7771"]),
            command("big", &[&format!("out:{BIG}"), &format!("err:{BIG}")]),
        ],
    );
    let counts: Vec<_> = report
        .results
        .iter()
        .map(|result: &CommandResult| (result.stdout_bytes, result.stderr_bytes))
        .collect();
    assert_eq!(counts, [(123_457, 7771), (BIG, BIG)]);
    // A result holds counts only; the summary not even those.
    assert!(!report.summary.contains("123457") && !report.summary.contains(&BIG.to_string()));
}

fn cancel_ends_the_tree_and_the_batch() {
    let dir = TestDir::create("cancel");
    let (grandchild, late, next) = (
        dir.path("grandchild"),
        dir.path("grandchild-late"),
        dir.path("next"),
    );
    let cancel = Cancel::default();
    let trigger = cancel.clone();
    let armed = grandchild.clone();
    thread::spawn(move || {
        while !Path::new(&armed).exists() {
            thread::sleep(Duration::from_millis(5));
        }
        trigger.cancel();
    });
    let prepared = prepare(
        &dir.0,
        &[
            command(
                "tree",
                &[
                    "spawn",
                    "[",
                    "spawn",
                    "[",
                    &format!("mark:{grandchild}"),
                    &format!("sleep:{LATE_MS}"),
                    &format!("mark:{late}"),
                    "]",
                    "sleep:30000",
                    "]",
                    "sleep:30000",
                ],
            ),
            command("next", &[&format!("mark:{next}")]),
        ],
    )
    .expect("valid");
    let started = Instant::now();
    assert_eq!(prepared.run(&cancel), Err(Cancelled));
    assert!(started.elapsed() < Duration::from_secs(10));
    thread::sleep(Duration::from_millis(LATE_MS + 1000));
    assert!(Path::new(&grandchild).exists(), "the tree never started");
    assert!(!Path::new(&late).exists(), "the grandchild outlived cancel");
    assert!(!Path::new(&next).exists(), "a command ran after cancel");

    // Cancelled before the first command: nothing is spawned.
    let first = dir.path("first");
    let prepared = prepare(&dir.0, &[command("first", &[&format!("mark:{first}")])]).expect("ok");
    assert_eq!(prepared.run(&cancel), Err(Cancelled));
    assert!(!Path::new(&first).exists());
}
