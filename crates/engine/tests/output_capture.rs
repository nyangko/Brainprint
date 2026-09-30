//! #53 step 1: opt-in capture on the common runner's sink seam, driven by
//! this test binary run as a portable helper (no shell), as in
//! `process_runner.rs`. `harness = false` keeps libtest off the helper's
//! stdout.
//!
//! Helper ops, one argument each, run in order: `exit:<code>`,
//! `sleep:<ms>`, `out:<bytes>` / `err:<bytes>` (a generated pattern),
//! `errline:<text>` (the text and a newline on stderr).

use std::{fs, io::Write, path::PathBuf, process::Command, thread, time::Duration};

use brainprint_engine::{
    diagnostics::{DiagnosticFormat, Severity, Stream},
    output_capture::{
        CaptureRequest, CaptureStatus, CapturedRun, MAX_HEAD, MAX_TAIL, run_captured,
    },
    process_runner::{Cancel, RunEnd},
};

const HELPER: &str = "--brainprint-capture-helper";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(HELPER) {
        helper(&args[1..]);
        return;
    }
    let tests: &[(&str, fn())] = &[
        (
            "over_64_mib_keeps_exact_head_and_tail",
            over_64_mib_keeps_exact_head_and_tail,
        ),
        (
            "diagnostics_are_apart_from_the_exit",
            diagnostics_are_apart_from_the_exit,
        ),
        (
            "a_timed_out_run_is_a_partial_capture",
            a_timed_out_run_is_a_partial_capture,
        ),
        (
            "nothing_requested_keeps_nothing",
            nothing_requested_keeps_nothing,
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

fn helper(ops: &[String]) {
    for op in ops {
        let (name, value) = op.split_once(':').expect("op:value");
        match name {
            "exit" => std::process::exit(value.parse().expect("code")),
            "sleep" => thread::sleep(Duration::from_millis(value.parse().expect("ms"))),
            "out" => write_pattern(&mut std::io::stdout(), value),
            "err" => write_pattern(&mut std::io::stderr(), value),
            "errline" => writeln!(std::io::stderr(), "{value}").expect("write"),
            other => panic!("unknown helper op {other}"),
        }
    }
}

/// Byte `i` of a generated stream; 251 is prime, so no power-of-two
/// offset error hides.
fn pattern(i: u64) -> u8 {
    (i % 251) as u8
}

fn write_pattern(stream: &mut impl Write, count: &str) {
    let total: u64 = count.parse().expect("bytes");
    let mut chunk = vec![0_u8; 64 * 1024];
    let mut at = 0;
    while at < total {
        let now = (total - at).min(chunk.len() as u64) as usize;
        for (offset, byte) in chunk[..now].iter_mut().enumerate() {
            *byte = pattern(at + offset as u64);
        }
        stream.write_all(&chunk[..now]).expect("write");
        at += now as u64;
    }
    stream.flush().expect("flush");
}

// --------------------------------------------------------------- harness

struct Workspace(PathBuf);

impl Workspace {
    fn create(label: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("brainprint-capture-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).expect("dirs");
        fs::write(dir.join("src/main.rs"), "").expect("file");
        Self(fs::canonicalize(&dir).expect("canonical"))
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn capture(ops: &[&str], timeout: Duration, request: CaptureRequest) -> CapturedRun {
    let workspace =
        Workspace::create(&format!("{:?}", thread::current().id()).replace(['(', ')'], ""));
    let mut command = Command::new(std::env::current_exe().expect("exe"));
    command.arg(HELPER).args(ops);
    run_captured(
        &mut command,
        timeout,
        &Cancel::default(),
        request,
        &workspace.0,
        &workspace.0,
    )
    .expect("spawned")
}

fn assert_pattern(bytes: &[u8], start: u64) {
    assert!(
        bytes
            .iter()
            .enumerate()
            .all(|(offset, &byte)| byte == pattern(start + offset as u64)),
        "bytes from {start} differ"
    );
}

// ------------------------------------------------------------------ tests

fn over_64_mib_keeps_exact_head_and_tail() {
    const BIG: u64 = 64 * 1024 * 1024 + 12_345;
    const SMALL: u64 = 3 * 1024 * 1024 + 1;
    let run = capture(
        &[&format!("out:{BIG}"), &format!("err:{SMALL}")],
        Duration::from_secs(120),
        CaptureRequest {
            raw: true,
            diagnostics: None,
        },
    );
    assert!(matches!(run.output.end, RunEnd::Exited(status) if status.success()));
    assert_eq!(run.capture.status, CaptureStatus::Complete);
    let stdout = run.capture.stdout.expect("stdout kept");
    // The capture saw exactly what the runner counted.
    assert_eq!(stdout.observed_bytes, run.output.stdout.bytes);
    assert_eq!(stdout.observed_bytes, BIG);
    assert!(stdout.truncated);
    assert_eq!(stdout.omitted_bytes, BIG - (MAX_HEAD + MAX_TAIL) as u64);
    assert_eq!(stdout.tail_start_offset, BIG - MAX_TAIL as u64);
    assert_eq!(stdout.head().len(), MAX_HEAD);
    assert_eq!(stdout.tail().len(), MAX_TAIL);
    assert_pattern(stdout.head(), 0);
    assert_pattern(stdout.tail(), stdout.tail_start_offset);
    let stderr = run.capture.stderr.expect("stderr kept");
    assert_eq!(stderr.observed_bytes, SMALL);
    assert!(!stderr.truncated);
    assert_eq!(stderr.head().len() as u64, SMALL);
    assert_pattern(stderr.head(), 0);
    assert!(run.capture.diagnostics.is_none());
}

fn diagnostics_are_apart_from_the_exit() {
    let request = CaptureRequest {
        raw: false,
        diagnostics: Some(DiagnosticFormat::PathLineColumn),
    };
    let failed = capture(
        &[
            "errline:src/main.rs:10:4: error: something failed",
            "errline:not a diagnostic",
            "exit:3",
        ],
        Duration::from_secs(20),
        request,
    );
    assert!(matches!(failed.output.end, RunEnd::Exited(status) if status.code() == Some(3)));
    assert_eq!(failed.capture.status, CaptureStatus::Complete);
    assert!(failed.capture.stdout.is_none() && failed.capture.stderr.is_none());
    let summary = failed.capture.diagnostics.expect("diagnostics");
    assert_eq!((summary.observed, summary.parse_misses), (1, 1));
    let item = &summary.items[0];
    assert_eq!(
        (item.severity, item.path.as_deref(), item.line, item.column),
        (Severity::Error, Some("src/main.rs"), Some(10), Some(4))
    );
    assert_eq!((item.stream, item.external), (Stream::Stderr, false));

    // Output the format cannot read never fails a command that passed.
    let passed = capture(
        &["errline:{not json", "exit:0"],
        Duration::from_secs(20),
        CaptureRequest {
            raw: false,
            diagnostics: Some(DiagnosticFormat::RustcJson),
        },
    );
    assert!(matches!(passed.output.end, RunEnd::Exited(status) if status.success()));
    let summary = passed.capture.diagnostics.expect("diagnostics");
    assert!(summary.items.is_empty());
    assert_eq!(summary.parse_misses, 1);
}

fn a_timed_out_run_is_a_partial_capture() {
    let run = capture(
        &["out:1000", "sleep:30000"],
        Duration::from_millis(1500),
        CaptureRequest {
            raw: true,
            diagnostics: Some(DiagnosticFormat::PathLineColumn),
        },
    );
    assert_eq!(run.output.end, RunEnd::TimedOut);
    assert_eq!(run.capture.status, CaptureStatus::Partial);
    let stdout = run.capture.stdout.expect("stdout kept");
    assert_eq!(stdout.observed_bytes, 1000);
    assert_pattern(stdout.head(), 0);
}

fn nothing_requested_keeps_nothing() {
    let run = capture(
        &["out:5000", "err:5000"],
        Duration::from_secs(20),
        CaptureRequest {
            raw: false,
            diagnostics: None,
        },
    );
    assert_eq!(
        (run.output.stdout.bytes, run.output.stderr.bytes),
        (5000, 5000)
    );
    assert!(run.output.stdout.kept.is_empty());
    assert_eq!(run.capture.status, CaptureStatus::Complete);
    assert!(run.capture.stdout.is_none() && run.capture.stderr.is_none());
    assert!(run.capture.diagnostics.is_none());
}
