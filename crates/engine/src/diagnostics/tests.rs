use std::{fs, path::PathBuf};

use super::*;
use crate::git_status::plain_path;

fn parse(format: DiagnosticFormat, input: &str) -> StreamDiagnostics {
    let mut parser = DiagnosticParser::new(format, Stream::Stdout);
    // Byte-sized chunks: nothing may depend on where a chunk ends.
    for byte in input.as_bytes() {
        parser.write(std::slice::from_ref(byte));
    }
    parser.finish(true)
}

fn found(format: DiagnosticFormat, input: &str) -> Vec<Found> {
    parse(format, input).kept.items
}

fn plc(line: &str) -> Option<Found> {
    let mut items = found(DiagnosticFormat::PathLineColumn, line);
    assert!(items.len() <= 1);
    items.pop()
}

fn rustc(line: &str) -> Option<Found> {
    let mut items = found(DiagnosticFormat::RustcJson, line);
    assert!(items.len() <= 1);
    items.pop()
}

fn diag(severity: Severity, message: &str) -> Found {
    Found {
        severity,
        code: None,
        message: message.to_owned(),
        message_truncated: false,
        path: Some("src/a.rs".to_owned()),
        line: Some(1),
        column: Some(1),
    }
}

// ------------------------------------------------------------ PathLineColumn

#[test]
fn path_line_column_lines() {
    let unix = plc("src/main.rs:10:4: error: something failed").expect("unix");
    assert_eq!(
        unix,
        Found {
            severity: Severity::Error,
            code: None,
            message: "something failed".to_owned(),
            message_truncated: false,
            path: Some("src/main.rs".to_owned()),
            line: Some(10),
            column: Some(4),
        }
    );
    let spaced = plc("src/a b.ts:20:7: warning: problem").expect("space");
    assert_eq!(
        (
            spaced.path.as_deref(),
            spaced.severity,
            spaced.line,
            spaced.column
        ),
        (Some("src/a b.ts"), Severity::Warning, Some(20), Some(7))
    );
    let windows = plc(r"C:\work\repo\src\main.rs:12:8: error: problem").expect("windows");
    assert_eq!(
        (windows.path.as_deref(), windows.line, windows.column),
        (Some(r"C:\work\repo\src\main.rs"), Some(12), Some(8))
    );
    assert_eq!(windows.message, "problem");
    let absolute = plc("/abs/x.c:3:5: note: here\r").expect("absolute, CRLF");
    assert_eq!(
        (
            absolute.path.as_deref(),
            absolute.severity,
            absolute.message.as_str()
        ),
        (Some("/abs/x.c"), Severity::Note, "here")
    );
    let relative = plc("../lib/y.py:1:1: help: try this").expect("relative");
    assert_eq!(relative.severity, Severity::Help);
    let coded = plc("src/lib.rs:2:5: error[E0308]: mismatched types").expect("code");
    assert_eq!(
        (
            coded.severity,
            coded.code.as_deref(),
            coded.message.as_str()
        ),
        (Severity::Error, Some("E0308"), "mismatched types")
    );
    // No marker: Unknown, the message untouched, no code guessed.
    let plain = plc("src/a.rs:1:2: Error: E1 happened").expect("plain");
    assert_eq!(
        (plain.severity, plain.code, plain.message.as_str()),
        (Severity::Unknown, None, "Error: E1 happened")
    );
}

#[test]
fn ambiguous_lines_are_parse_misses() {
    for line in [
        "not a diagnostic",
        "foo:bar:baz",
        "12:30:45: done",
        "2026-09-30T12:30:45: started",
        "Sep 30 12:30:45: started",
        "v1.2 build 12:30:45: ok",
        "12.5:3:4: ratio",
        "src/main.rs:10: error: no column",
        "src/main.rs:10:4:error: no space",
        "src/main.rs:10:4: ",
        "src/main.rs:10:4: error: ",
        "src/main.rs:x:4: error: bad line",
        "src/main.rs:99999999999:4: error: overflow",
        " src/main.rs:1:2: error: leading space",
        "error: src/main.rs:1:2: prefixed",
        "C:work.rs:1:2: error: drive without separator",
        "Makefile:1:2: no extension",
        "",
    ] {
        let parsed = parse(DiagnosticFormat::PathLineColumn, line);
        assert!(parsed.kept.items.is_empty(), "{line:?}");
        assert_eq!(parsed.parse_misses, u64::from(!line.is_empty()), "{line:?}");
    }
}

#[test]
fn every_line_is_a_diagnostic_or_a_miss() {
    let parsed = parse(
        DiagnosticFormat::PathLineColumn,
        "a.rs:1:1: error: one\nnoise\n\nb.rs:2:2: warning: two",
    );
    assert_eq!(parsed.kept.items.len(), 2);
    assert_eq!(parsed.parse_misses, 2);
    // A last line cut off by the end of the run is not parsed.
    let mut parser = DiagnosticParser::new(DiagnosticFormat::PathLineColumn, Stream::Stderr);
    parser.write(b"a.rs:1:1: error: cut");
    let cut = parser.finish(false);
    assert!(cut.kept.items.is_empty());
    assert_eq!(cut.parse_misses, 1);
}

#[test]
fn an_overlong_path_is_a_miss_not_a_cut_path() {
    let line = format!("{}.rs:1:1: error: x", "p".repeat(MAX_MESSAGE_BYTES));
    assert_eq!(plc(&line), None);
    let fits = format!("{}.rs:1:1: error: x", "p".repeat(MAX_MESSAGE_BYTES - 3));
    assert!(plc(&fits).is_some());
}

// ----------------------------------------------------------------- RustcJson

fn compiler_message(diagnostic: &str) -> String {
    format!(
        r#"{{"reason":"compiler-message","package_id":"x 0.1.0","manifest_path":"/w/Cargo.toml","target":{{"kind":["lib"],"name":"x","src_path":"/w/src/lib.rs","edition":"2021","doc":true,"doctest":true,"test":true}},"message":{diagnostic}}}"#
    )
}

fn span(file: &str, line: u32, column: u32, primary: bool) -> String {
    format!(
        r#"{{"byte_end":10,"byte_start":5,"column_end":9,"column_start":{column},"expansion":null,"file_name":"{file}","is_primary":{primary},"label":"expected `u32`","line_end":{line},"line_start":{line},"suggested_replacement":null,"suggestion_applicability":null,"text":[{{"highlight_end":9,"highlight_start":5,"text":"    let x: u32 = \"a\";"}}]}}"#
    )
}

const RENDERED: &str = r#""error[E0308]: mismatched types\n --> src/lib.rs:2:18\n  |\n2 |     let x: u32 = \"a\";\n  |                  ^^^ expected `u32`\n\n""#;

#[test]
fn rustc_error_with_code_and_primary_span() {
    let line = compiler_message(&format!(
        r#"{{"$message_type":"diagnostic","children":[{{"children":[],"code":null,"level":"note","message":"child note","rendered":null,"spans":[{}]}}],"code":{{"code":"E0308","explanation":"Expected type did not match the received type.\n\nErroneous code examples:\n\n```\nlet x: i32 = \"I am not a number!\";\n```\n"}},"level":"error","message":"mismatched types","spans":[{},{}],"rendered":{RENDERED}}}"#,
        span("src/child.rs", 9, 9, true),
        span("src/other.rs", 7, 3, false),
        span("src/lib.rs", 2, 18, true),
    ));
    assert_eq!(
        rustc(&line),
        Some(Found {
            severity: Severity::Error,
            code: Some("E0308".to_owned()),
            message: "mismatched types".to_owned(),
            message_truncated: false,
            path: Some("src/lib.rs".to_owned()),
            line: Some(2),
            column: Some(18),
        })
    );
}

#[test]
fn rustc_warning_without_code_or_primary_span() {
    let no_code = compiler_message(
        r#"{"children":[],"code":null,"level":"warning","message":"2 warnings emitted","spans":[],"rendered":"warning: 2 warnings emitted\n\n"}"#,
    );
    assert_eq!(
        rustc(&no_code),
        Some(Found {
            severity: Severity::Warning,
            code: None,
            message: "2 warnings emitted".to_owned(),
            message_truncated: false,
            path: None,
            line: None,
            column: None,
        })
    );
    let lint = compiler_message(&format!(
        r#"{{"children":[],"code":{{"code":"unused_variables","explanation":null}},"level":"warning","message":"unused variable: `y`","spans":[{}],"rendered":"w"}}"#,
        span("src/lib.rs", 3, 9, false),
    ));
    let lint = rustc(&lint).expect("lint");
    assert_eq!(lint.code.as_deref(), Some("unused_variables"));
    // Spans, but none primary: no location is invented.
    assert_eq!((lint.path, lint.line, lint.column), (None, None, None));
}

#[test]
fn a_huge_rendered_field_is_scanned_past_not_kept() {
    let huge = "x".repeat(4 * 1024 * 1024);
    let line = compiler_message(&format!(
        r#"{{"rendered":"{huge}","children":[],"code":null,"level":"error","message":"boom","spans":[{}]}}"#,
        span("src/lib.rs", 1, 1, true),
    ));
    let found = rustc(&line).expect("parsed");
    assert_eq!(found.message, "boom");
    assert!(!found.message.contains("xxx"));
}

#[test]
fn non_diagnostic_and_malformed_lines_are_misses() {
    let artifact = r#"{"reason":"compiler-artifact","package_id":"x","target":{"name":"x"},"profile":{"opt_level":"0"},"features":[],"filenames":["/w/target/libx.rlib"],"executable":null,"fresh":true}"#;
    let finished = r#"{"reason":"build-finished","success":false}"#;
    let valid =
        compiler_message(r#"{"children":[],"code":null,"level":"error","message":"m","spans":[]}"#);
    for line in [
        artifact.to_owned(),
        finished.to_owned(),
        "error[E0308]: mismatched types".to_owned(),
        "not json".to_owned(),
        "[1,2,3]".to_owned(),
        valid[..valid.len() - 1].to_owned(),
        format!("{valid}}}"),
        format!("{valid} x"),
        valid.replace(r#""level""#, r#""level"x"#),
        valid.replace(r#""m""#, r#""m\q""#),
        valid.replace(r#""m""#, "\"m\u{1}\""),
        valid.replace(r#""m""#, r#""\ud800""#),
        valid.replace("[]", "[01]"),
        valid.replace("[]", "[1.]"),
        valid.replace("[]", "[-]"),
        valid.replace("[]", "[tru]"),
        valid.replace("[]", "[}"),
        compiler_message(r#"{"children":[],"code":null,"message":"no level","spans":[]}"#),
        compiler_message(r#"{"children":[],"code":null,"level":"error","spans":[]}"#),
        format!(
            r#"{{"a":{}{}}}"#,
            "[".repeat(MAX_JSON_DEPTH),
            "]".repeat(MAX_JSON_DEPTH)
        ),
    ] {
        let parsed = parse(DiagnosticFormat::RustcJson, &line);
        assert!(parsed.kept.items.is_empty(), "{line}");
        assert_eq!(parsed.parse_misses, 1, "{line}");
    }
    let parsed = parse(DiagnosticFormat::RustcJson, &valid);
    assert_eq!((parsed.kept.items.len(), parsed.parse_misses), (1, 0));
}

#[test]
fn json_escapes_numbers_and_nesting_are_read_exactly() {
    let line = compiler_message(&format!(
        r#"{{"x":[-1.5e+3,0,1E2,true,false,null,{{}},[]],"children":[{{"spans":[{}]}}],"code":null,"level":"error","message":"a \"q\" \\ \/ \t \u00e9 \ud83d\ude00","spans":[{}]}}"#,
        span("src/nested.rs", 5, 5, true),
        span(r"src\\win.rs", 4294967295, 7, true),
    ));
    let found = rustc(&line).expect("parsed");
    assert_eq!(found.message, "a \"q\" \\ / \t é 😀");
    assert_eq!(found.path.as_deref(), Some(r"src\win.rs"));
    assert_eq!((found.line, found.column), (Some(u32::MAX), Some(7)));
}

#[test]
fn an_overlong_code_or_path_is_a_miss() {
    let long = "c".repeat(MAX_MESSAGE_BYTES + 1);
    let code = compiler_message(&format!(
        r#"{{"children":[],"code":{{"code":"{long}"}},"level":"error","message":"m","spans":[]}}"#
    ));
    assert_eq!(rustc(&code), None);
    let path = compiler_message(&format!(
        r#"{{"children":[],"code":null,"level":"error","message":"m","spans":[{}]}}"#,
        span(&long, 1, 1, true)
    ));
    assert_eq!(rustc(&path), None);
}

// ---------------------------------------------------------- message bounds

#[test]
fn a_long_message_is_cut_on_a_char_boundary() {
    // 'é' is 2 bytes; an odd prefix puts the bound mid-character.
    let message = format!("x{}", "é".repeat(MAX_MESSAGE_BYTES));
    for found in [
        plc(&format!("a.rs:1:1: {message}")).expect("plc"),
        rustc(&compiler_message(&format!(
            r#"{{"code":null,"level":"error","message":"{message}","spans":[]}}"#
        )))
        .expect("rustc"),
    ] {
        assert!(found.message_truncated);
        assert!(found.message.len() <= MAX_MESSAGE_BYTES);
        assert_eq!(found.message.len(), MAX_MESSAGE_BYTES - 1);
        assert!(message.starts_with(&found.message));
    }
    let exact = "m".repeat(MAX_MESSAGE_BYTES);
    let found = plc(&format!("a.rs:1:1: {exact}")).expect("exact");
    assert!(!found.message_truncated);
    assert_eq!(found.message, exact);
}

// ------------------------------------------------------ dedupe / bound / order

#[test]
fn exact_duplicates_are_counted_once() {
    let parsed = parse(
        DiagnosticFormat::PathLineColumn,
        "a.rs:1:1: error: x\na.rs:1:1: error: x\na.rs:1:2: error: x\n",
    );
    assert_eq!(parsed.kept.items.len(), 2);
    assert_eq!((parsed.kept.observed, parsed.kept.deduplicated), (3, 1));
}

#[test]
fn severity_order_then_first_seen() {
    let input = [
        "a.rs:1:1: unknown one",
        "a.rs:1:1: help: h",
        "a.rs:1:1: note: n",
        "a.rs:1:1: warning: w1",
        "a.rs:1:1: error: e1",
        "a.rs:1:1: warning: w2",
        "a.rs:1:1: error: e2",
    ]
    .join("\n");
    let messages: Vec<_> = found(DiagnosticFormat::PathLineColumn, &input)
        .into_iter()
        .map(|found| found.message)
        .collect();
    assert_eq!(messages, ["e1", "e2", "w1", "w2", "n", "h", "unknown one"]);
}

#[test]
fn past_64_the_rest_is_omitted_and_better_severities_still_win() {
    let mut kept = Kept::default();
    for index in 0..MAX_DIAGNOSTICS {
        kept.push(diag(Severity::Warning, &format!("w{index}")));
    }
    assert_eq!((kept.items.len(), kept.omitted), (64, 0));
    kept.push(diag(Severity::Warning, "w64"));
    assert_eq!((kept.items.len(), kept.omitted), (64, 1));
    assert!(kept.items.iter().all(|found| found.message != "w64"));
    // An error displaces the last warning.
    kept.push(diag(Severity::Error, "e"));
    assert_eq!(kept.items[0].message, "e");
    assert_eq!(kept.items[63].message, "w62");
    assert_eq!(kept.omitted, 2);
    // A duplicate of a kept item is still deduplicated when full.
    kept.push(diag(Severity::Warning, "w0"));
    assert_eq!((kept.deduplicated, kept.omitted), (1, 2));
    assert_eq!(
        kept.observed,
        kept.items.len() as u64 + kept.deduplicated + kept.omitted
    );
}

#[test]
fn over_64_rustc_diagnostics_are_bounded() {
    let input: Vec<String> = (0..100)
        .map(|index| {
            compiler_message(&format!(
                r#"{{"code":null,"level":"warning","message":"w{index}","spans":[]}}"#
            ))
        })
        .collect();
    let parsed = parse(DiagnosticFormat::RustcJson, &input.join("\n"));
    assert_eq!(parsed.kept.items.len(), 64);
    assert_eq!((parsed.kept.observed, parsed.kept.omitted), (100, 36));
    assert_eq!(parsed.kept.items[63].message, "w63");
}

// ----------------------------------------------------------------- summarize

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn workspace(label: &str) -> (Dir, PathBuf) {
    let dir = std::env::temp_dir().join(format!("brainprint-diag-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("ws/src/sub")).expect("dirs");
    fs::write(dir.join("ws/src/sub/a.rs"), "").expect("file");
    fs::write(dir.join("outside.rs"), "").expect("file");
    let root = fs::canonicalize(dir.join("ws")).expect("root");
    (Dir(dir), root)
}

#[test]
fn summarize_merges_bounds_and_normalizes_paths() {
    let (_dir, root) = workspace("summary");
    let base = root.join("src");
    let outside = root.parent().expect("parent").join("outside.rs");
    let stdout = parse(
        DiagnosticFormat::PathLineColumn,
        &format!(
            "sub/a.rs:1:1: warning: relative to cwd\n{}:2:2: error: absolute inside\n\
             ../../outside.rs:3:3: error: outside\n/rustc/abc/core.rs:4:4: note: virtual\nnoise",
            plain_path(&root.join("src/sub/a.rs")).display()
        ),
    );
    let mut stderr = DiagnosticParser::new(DiagnosticFormat::PathLineColumn, Stream::Stderr);
    stderr.write(
        format!(
            "{}:5:5: error: absolute outside\n",
            plain_path(&outside).display()
        )
        .as_bytes(),
    );
    let summary = summarize([stdout, stderr.finish(true)], &root, &base);
    assert_eq!(
        (
            summary.observed,
            summary.deduplicated,
            summary.omitted,
            summary.parse_misses
        ),
        (5, 0, 0, 1)
    );
    let view: Vec<_> = summary
        .items
        .iter()
        .map(|d| (d.message.as_str(), d.path.as_deref(), d.external, d.stream))
        .collect();
    assert_eq!(
        view,
        [
            (
                "absolute inside",
                Some("src/sub/a.rs"),
                false,
                Stream::Stdout
            ),
            ("outside", None, true, Stream::Stdout),
            ("absolute outside", None, true, Stream::Stderr),
            (
                "relative to cwd",
                Some("src/sub/a.rs"),
                false,
                Stream::Stdout
            ),
            ("virtual", None, true, Stream::Stdout),
        ]
    );
}

#[test]
fn summarize_bounds_the_merged_streams_to_64() {
    let (_dir, root) = workspace("merge");
    let lines = |severity: &str| {
        (0..40)
            .map(|index| format!("src/sub/a.rs:{index}:1: {severity}: m"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let stdout = parse(DiagnosticFormat::PathLineColumn, &lines("warning"));
    let mut stderr = DiagnosticParser::new(DiagnosticFormat::PathLineColumn, Stream::Stderr);
    stderr.write(lines("error").as_bytes());
    let summary = summarize([stdout, stderr.finish(true)], &root, &root);
    assert_eq!(summary.items.len(), 64);
    assert_eq!((summary.observed, summary.omitted), (80, 16));
    assert!(
        summary.items[..40]
            .iter()
            .all(|d| d.severity == Severity::Error)
    );
    assert_eq!(summary.items[40].line, Some(0));
    assert_eq!(summary.items[63].line, Some(23));
}
