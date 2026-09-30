//! #52 step 1: the common bounded runner, driven by this test binary run
//! as a portable helper (no shell), so the same tests hold on macOS,
//! Ubuntu and Windows. `harness = false`: libtest would write its own
//! lines to the helper's stdout and spoil the byte counts.
//!
//! Helper ops, one argument each, run in order:
//! `exit:<code>`, `sleep:<ms>`, `out:<bytes>`, `err:<bytes>`,
//! `mark:<path>` (create a file), `await:<path>` (wait until it exists),
//! `spawn [ ops… ]` (a child sharing our stdout/stderr, not waited for),
//! `detach [ ops… ]` (the same with stdio null).

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use brainprint_engine::process_runner::{Cancel, Capture, RunEnd, RunOutput, run};

const HELPER: &str = "--brainprint-runner-helper";
/// Deadline for the tree tests; long enough for a slow CI to start them.
const DEADLINE: Duration = Duration::from_secs(2);
/// Longer than any deadline: a helper still running would write it late.
const LATE_MS: u64 = 4000;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(HELPER) {
        helper(&args[1..]);
        return;
    }
    let tests: &[(&str, fn())] = &[
        (
            "exit_codes_and_exact_byte_counts",
            exit_codes_and_exact_byte_counts,
        ),
        (
            "timeout_ends_child_and_grandchild",
            timeout_ends_child_and_grandchild,
        ),
        ("cancel_ends_the_tree", cancel_ends_the_tree),
        (
            "descendants_left_after_a_normal_exit_are_ended",
            descendants_left_after_a_normal_exit_are_ended,
        ),
        (
            "a_descendant_holding_the_pipes_cannot_outlive_the_deadline",
            a_descendant_holding_the_pipes_cannot_outlive_the_deadline,
        ),
        (
            "output_over_64_mib_is_counted_not_kept",
            output_over_64_mib_is_counted_not_kept,
        ),
        (
            "keep_and_limit_bound_what_is_kept",
            keep_and_limit_bound_what_is_kept,
        ),
        (
            "an_unstartable_program_is_a_spawn_error",
            an_unstartable_program_is_a_spawn_error,
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
        if op == "spawn" || op == "detach" {
            let end = closing(ops, index);
            let mut child = Command::new(std::env::current_exe().expect("exe"));
            child.arg(HELPER).args(&ops[index + 1..end]);
            if op == "detach" {
                child
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
            }
            child.spawn().expect("spawn helper");
            index = end + 1;
            continue;
        }
        let (name, value) = op.split_once(':').expect("op:value");
        match name {
            "exit" => std::process::exit(value.parse().expect("code")),
            "sleep" => thread::sleep(Duration::from_millis(value.parse().expect("ms"))),
            "out" => write_zeros(&mut std::io::stdout(), value),
            "err" => write_zeros(&mut std::io::stderr(), value),
            "mark" => fs::write(value, b"").expect("marker"),
            "await" => {
                let until = Instant::now() + Duration::from_secs(10);
                while !Path::new(value).exists() && Instant::now() < until {
                    thread::sleep(Duration::from_millis(5));
                }
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

fn helper_command(ops: &[&str]) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("exe"));
    command.arg(HELPER).args(ops);
    command
}

fn run_helper(ops: &[&str], timeout: Duration, cancel: &Cancel) -> RunOutput {
    run(
        &mut helper_command(ops),
        timeout,
        cancel,
        Capture::Count,
        Capture::Count,
    )
    .expect("spawned")
}

struct TestDir(PathBuf);

impl TestDir {
    fn create(label: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("brainprint-runner-{label}-{}", std::process::id()));
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

/// The started marker exists (the process was really alive) and the late
/// one never appears, even after the late write would have happened.
fn assert_ended(started: &str, late: &str) {
    assert!(Path::new(started).exists(), "{started} never started");
    thread::sleep(Duration::from_millis(LATE_MS + 1000));
    assert!(!Path::new(late).exists(), "{late} outlived the run");
}

fn assert_quick(started: Instant, bound: Duration) {
    assert!(started.elapsed() < bound, "took {:?}", started.elapsed());
}

// ----------------------------------------------------------------- tests

fn exit_codes_and_exact_byte_counts() {
    let output = run_helper(
        &["out:123457", "err:7771", "exit:7"],
        Duration::from_secs(20),
        &Cancel::default(),
    );
    let RunEnd::Exited(status) = output.end else {
        panic!("{:?}", output.end)
    };
    assert_eq!(status.code(), Some(7));
    assert_eq!((output.stdout.bytes, output.stderr.bytes), (123_457, 7771));
    assert!(output.stdout.complete && output.stderr.complete);
    assert!(output.stdout.kept.is_empty() && output.stderr.kept.is_empty());

    let ok = run_helper(&[], Duration::from_secs(20), &Cancel::default());
    assert!(matches!(ok.end, RunEnd::Exited(status) if status.success()));
    assert_eq!((ok.stdout.bytes, ok.stderr.bytes), (0, 0));
}

fn timeout_ends_child_and_grandchild() {
    let dir = TestDir::create("timeout");
    let (child, grandchild) = (dir.path("child"), dir.path("grandchild"));
    let (child_late, grandchild_late) = (dir.path("child-late"), dir.path("grandchild-late"));
    let late = format!("sleep:{LATE_MS}");
    let started = Instant::now();
    let output = run_helper(
        &[
            "spawn",
            "[",
            "spawn",
            "[",
            &format!("mark:{grandchild}"),
            &late,
            &format!("mark:{grandchild_late}"),
            "]",
            &format!("mark:{child}"),
            &late,
            &format!("mark:{child_late}"),
            "]",
            &format!("await:{grandchild}"),
            "sleep:30000",
        ],
        DEADLINE,
        &Cancel::default(),
    );
    assert_eq!(output.end, RunEnd::TimedOut);
    assert_quick(started, DEADLINE + Duration::from_secs(1));
    assert_ended(&child, &child_late);
    assert_ended(&grandchild, &grandchild_late);
}

fn cancel_ends_the_tree() {
    let dir = TestDir::create("cancel");
    let (grandchild, late) = (dir.path("grandchild"), dir.path("grandchild-late"));
    let cancel = Cancel::default();
    let trigger = cancel.clone();
    let armed = grandchild.clone();
    thread::spawn(move || {
        while !Path::new(&armed).exists() {
            thread::sleep(Duration::from_millis(5));
        }
        trigger.cancel();
    });
    let started = Instant::now();
    let output = run_helper(
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
        Duration::from_secs(60),
        &cancel,
    );
    assert_eq!(output.end, RunEnd::Cancelled);
    assert_quick(started, Duration::from_secs(10));
    assert_ended(&grandchild, &late);
}

fn descendants_left_after_a_normal_exit_are_ended() {
    let dir = TestDir::create("leftover");
    let (started_marker, late) = (dir.path("descendant"), dir.path("descendant-late"));
    let started = Instant::now();
    let output = run_helper(
        &[
            "detach",
            "[",
            &format!("mark:{started_marker}"),
            &format!("sleep:{LATE_MS}"),
            &format!("mark:{late}"),
            "]",
            &format!("await:{started_marker}"),
            "exit:0",
        ],
        Duration::from_secs(20),
        &Cancel::default(),
    );
    assert!(matches!(output.end, RunEnd::Exited(status) if status.success()));
    assert!(output.stdout.complete && output.stderr.complete);
    assert_quick(started, Duration::from_secs(5));
    assert_ended(&started_marker, &late);
}

fn a_descendant_holding_the_pipes_cannot_outlive_the_deadline() {
    let dir = TestDir::create("holder");
    let (holder, late) = (dir.path("holder"), dir.path("holder-late"));
    let started = Instant::now();
    let output = run_helper(
        &[
            "spawn",
            "[",
            &format!("mark:{holder}"),
            &format!("sleep:{LATE_MS}"),
            &format!("mark:{late}"),
            "]",
            &format!("await:{holder}"),
            "out:5",
            "exit:0",
        ],
        DEADLINE,
        &Cancel::default(),
    );
    assert!(matches!(output.end, RunEnd::Exited(status) if status.success()));
    assert!(!output.stdout.complete && !output.stderr.complete);
    assert_eq!(output.stdout.bytes, 5);
    assert_quick(started, DEADLINE + Duration::from_secs(1));
    assert_ended(&holder, &late);
}

fn output_over_64_mib_is_counted_not_kept() {
    const BIG: u64 = 64 * 1024 * 1024 + 12_345;
    let output = run_helper(
        &[&format!("out:{BIG}"), &format!("err:{BIG}")],
        Duration::from_secs(120),
        &Cancel::default(),
    );
    assert!(matches!(output.end, RunEnd::Exited(status) if status.success()));
    assert_eq!((output.stdout.bytes, output.stderr.bytes), (BIG, BIG));
    // Count keeps nothing: the only buffer is the reader's fixed chunk.
    assert!(output.stdout.kept.is_empty() && output.stderr.kept.is_empty());
}

fn keep_and_limit_bound_what_is_kept() {
    let kept = run(
        &mut helper_command(&["out:5000", "err:5000"]),
        Duration::from_secs(20),
        &Cancel::default(),
        Capture::Keep(5000),
        Capture::Keep(1000),
    )
    .expect("spawned");
    assert_eq!(kept.stdout.kept.len(), 5000);
    assert_eq!((kept.stderr.kept.len(), kept.stderr.bytes), (1000, 5000));

    let limited = run(
        &mut helper_command(&["out:200000", "sleep:30000"]),
        Duration::from_secs(20),
        &Cancel::default(),
        Capture::Limit(1000),
        Capture::Count,
    )
    .expect("spawned");
    assert_eq!(limited.end, RunEnd::OutputLimit);
    assert!(limited.stdout.kept.is_empty() && !limited.stdout.complete);
}

fn an_unstartable_program_is_a_spawn_error() {
    assert!(
        run(
            &mut Command::new("brainprint-no-such-program-52"),
            Duration::from_secs(5),
            &Cancel::default(),
            Capture::Count,
            Capture::Count,
        )
        .is_err()
    );
}
