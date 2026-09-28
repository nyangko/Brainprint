//! #46 measurement gate: whether pairing `textDocument/definition`
//! (exact target identity) with `textDocument/signatureHelp` (a bounded
//! probe position derived from the occurrence's own source range) can
//! confirm a macro-nested call that #45's reciprocal outgoingCalls
//! loses -- while rejecting `stringify!`/token-swallowing macros and
//! bare function-item references, and never using source punctuation
//! itself as CALLS evidence (only to choose the LSP probe position).
//!
//! Talks to a real, freshly spawned rust-analyzer over raw LSP, not
//! through `RustRequest`/`RustHost::call()`: #46 forbids production
//! wiring before this measurement proves the algorithm works.
//!
//! ```sh
//! cargo test -p brainprint-engine --test i46_signature_help_confirmation_measurement -- --ignored --nocapture
//! ```

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{self, Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};

use brainprint_engine::{
    lsp::jsonrpc::{Client, IgnoreServer, RpcFailure},
    runtime::CancelToken,
    rust_semantic::protocol::path_to_uri,
};
use serde_json::{Value, json};

fn fixture_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/workspaces/rust-semantic-spike")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("destination");
    for entry in fs::read_dir(from).expect("read fixture") {
        let entry = entry.expect("entry");
        let name = entry.file_name();
        if name == "target" || name == "build-rs-ran.marker" {
            continue;
        }
        let dest = to.join(&name);
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).expect("copy");
        }
    }
}

fn executable_or_skip() -> Option<PathBuf> {
    let which = Command::new("rustup")
        .args(["which", "rust-analyzer"])
        .output()
        .ok()?;
    if !which.status.success() {
        eprintln!("skipping #46 measurement: rust-analyzer is not installed");
        return None;
    }
    Some(PathBuf::from(String::from_utf8_lossy(&which.stdout).trim()))
}

struct Probe {
    client: Client,
    child: Child,
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.client.close();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Probe {
    fn start(executable: &Path, workspace_root: &Path) -> Self {
        let mut command = Command::new(executable);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(workspace_root)
            .env("CARGO_NET_OFFLINE", "true");
        let mut child = command.spawn().expect("spawn rust-analyzer");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut std::io::BufReader::new(stderr), &mut std::io::sink());
            });
        }
        let client = Client::new(Box::new(stdout), Box::new(stdin), Arc::new(IgnoreServer));

        let root_uri = path_to_uri(workspace_root);
        let cancel = CancelToken::new();
        client
            .request(
                "initialize",
                Some(json!({
                    "processId": process::id(),
                    "rootUri": root_uri,
                    "workspaceFolders": [{ "uri": root_uri, "name": "workspace" }],
                    "capabilities": {
                        "general": { "positionEncodings": ["utf-16"] },
                        "experimental": { "serverStatusNotification": true },
                        "textDocument": {
                            "definition": {},
                            "signatureHelp": {},
                        },
                    },
                })),
                &cancel,
            )
            .expect("initialize");
        client
            .notify("initialized", Some(json!({})))
            .expect("initialized");
        Self { client, child }
    }

    fn open(&self, uri: &str, text: &str) {
        self.client
            .notify(
                "textDocument/didOpen",
                Some(json!({
                    "textDocument": {
                        "uri": uri, "languageId": "rust", "version": 1, "text": text
                    }
                })),
            )
            .expect("didOpen");
    }

    fn request(&self, method: &str, params: Value) -> Result<Value, RpcFailure> {
        self.client
            .request(method, Some(params), &CancelToken::new())
    }
}

/// No settle barrier here on purpose: this measures raw capability, not
/// the product's quiescence contract. A fixed, generous sleep is the
/// whole tool.
fn let_it_load() {
    std::thread::sleep(Duration::from_secs(8));
}

fn read_to_string(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Byte offset -> zero-based line/UTF-16-character position, LSP's own
/// coordinate system. The fixture is ASCII, so UTF-16 code units equal
/// byte count for every substring here.
fn position_at_offset(text: &str, offset: usize) -> (u32, u32) {
    let before = &text[..offset];
    let line = before.matches('\n').count();
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let character = offset - line_start;
    (
        u32::try_from(line).unwrap(),
        u32::try_from(character).unwrap(),
    )
}

/// Find `needle` after the first occurrence of `anchor`, and return the
/// (start, end) LSP position of that needle occurrence. Anchoring on a
/// unique preceding string (a function's own `pub fn name` line) lets
/// every probe site share the identifier text `target_probe` /
/// `aliased_probe` without colliding with earlier occurrences in the
/// same file.
fn occurrence_after(text: &str, anchor: &str, needle: &str) -> ((u32, u32), (u32, u32)) {
    let anchor_at = text
        .find(anchor)
        .unwrap_or_else(|| panic!("{anchor:?} not found"));
    let needle_at = text[anchor_at..]
        .find(needle)
        .map(|offset| anchor_at + offset)
        .unwrap_or_else(|| panic!("{needle:?} not found after {anchor:?}"));
    (
        position_at_offset(text, needle_at),
        position_at_offset(text, needle_at + needle.len()),
    )
}

/// The probe-positioning constraint: source punctuation may only be
/// used to *choose* the signatureHelp position, never as CALLS evidence
/// by itself. This looks for a direct argument-list opener -- the next
/// non-space character after the occurrence's end -- and refuses to
/// cross a newline or any other token to find one. Returns the position
/// just inside the opening paren, where signatureHelp should be probed.
fn direct_argument_list_open_position(text: &str, ident_end: (u32, u32)) -> Option<(u32, u32)> {
    let line = text.lines().nth(ident_end.0 as usize)?;
    let mut chars = line.chars().skip(ident_end.1 as usize).peekable();
    let mut character = ident_end.1;
    loop {
        match chars.peek() {
            Some(' ') => {
                chars.next();
                character += 1;
            }
            Some('(') => return Some((ident_end.0, character + 1)),
            _ => return None,
        }
    }
}

/// `textDocument/definition`'s response shape varies (`Location`,
/// `Location[]`, or `LocationLink[]`); normalize to (uri, range) for
/// identity comparison, never by name.
fn definition_identity(response: &Value) -> Option<(String, Value)> {
    let first = if response.is_array() {
        response.as_array()?.first()?.clone()
    } else {
        response.clone()
    };
    if let Some(uri) = first.get("targetUri") {
        return Some((
            uri.as_str()?.to_string(),
            first["targetSelectionRange"].clone(),
        ));
    }
    Some((
        first.get("uri")?.as_str()?.to_string(),
        first["range"].clone(),
    ))
}

/// A confirmed CALL requires both: exact definition identity equal to
/// the original target, and a non-empty semantic signatureHelp at the
/// bounded probe position for that same occurrence.
fn signature_help_is_callable(response: &Value) -> bool {
    response
        .get("signatures")
        .and_then(Value::as_array)
        .is_some_and(|signatures| !signatures.is_empty())
}

struct Shape {
    label: &'static str,
    positive: bool,
    uri: String,
    text: String,
    anchor: &'static str,
    needle: &'static str,
}

#[test]
#[ignore = "needs an installed rust-analyzer; spawns a real process"]
fn signature_help_confirms_macro_nested_calls_and_rejects_non_calls() {
    let Some(executable) = executable_or_skip() else {
        return;
    };
    let base = env::temp_dir().join(format!("brainprint-i46-measure-{}", process::id()));
    let _ = fs::remove_dir_all(&base);
    copy_tree(&fixture_source(), &base);

    let probe = Probe::start(&executable, &base);

    let target_probe_path = base.join("crates/core/src/target_probe.rs");
    let target_probe_text = read_to_string(&target_probe_path);
    let target_probe_uri = path_to_uri(&target_probe_path);
    probe.open(&target_probe_uri, &target_probe_text);

    let runner_path = base.join("crates/core/src/runner.rs");
    let runner_text = read_to_string(&runner_path);
    probe.open(&path_to_uri(&runner_path), &runner_text);

    let main_path = base.join("crates/app/src/main.rs");
    let main_text = read_to_string(&main_path);
    probe.open(&path_to_uri(&main_path), &main_text);

    let_it_load();

    // Establish target identity via `textDocument/definition` on the
    // declaration itself -- the same method used for every occurrence
    // below, so comparison never mixes identity schemes.
    let (decl_start, _) =
        occurrence_after(&target_probe_text, "pub fn target_probe", "target_probe");
    let decl_definition = probe
        .request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": target_probe_uri },
                "position": { "line": decl_start.0, "character": decl_start.1 + 1 },
            }),
        )
        .expect("definition on declaration");
    let target_identity = definition_identity(&decl_definition)
        .expect("declaration definition did not resolve to an identity -- cannot measure");
    eprintln!("target identity: {target_identity:?}");

    let shapes = vec![
        Shape {
            label: "same_file_caller (positive: same-file call)",
            positive: true,
            uri: target_probe_uri.clone(),
            text: target_probe_text.clone(),
            anchor: "pub fn same_file_caller",
            needle: "target_probe",
        },
        Shape {
            label: "cross_file_caller (positive: cross-file call)",
            positive: true,
            uri: path_to_uri(&runner_path),
            text: runner_text.clone(),
            anchor: "pub fn cross_file_caller",
            needle: "target_probe",
        },
        Shape {
            label: "cross-crate call in main() (positive: sibling-workspace crate)",
            positive: true,
            uri: path_to_uri(&main_path),
            text: main_text.clone(),
            anchor: "let _cross_crate",
            needle: "target_probe",
        },
        Shape {
            label: "macro_nested_call_matches_the_target (positive: assert_eq! nesting)",
            positive: true,
            uri: target_probe_uri.clone(),
            text: target_probe_text.clone(),
            anchor: "fn macro_nested_call_matches_the_target",
            needle: "target_probe",
        },
        Shape {
            label: "nested_macro_call_matches_the_target (positive: assert!(matches!(..)) nesting)",
            positive: true,
            uri: target_probe_uri.clone(),
            text: target_probe_text.clone(),
            anchor: "fn nested_macro_call_matches_the_target",
            needle: "target_probe",
        },
        Shape {
            label: "mixed_call_and_reference_caller's call range (positive)",
            positive: true,
            uri: target_probe_uri.clone(),
            text: target_probe_text.clone(),
            anchor: "let value =",
            needle: "target_probe",
        },
        Shape {
            label: "aliased_import_caller (positive: alias, normal consumer module)",
            positive: true,
            uri: path_to_uri(&runner_path),
            text: runner_text.clone(),
            anchor: "pub fn aliased_import_caller",
            needle: "aliased_probe",
        },
        Shape {
            label: "not_a_call_reference_only (negative: isolated bare reference)",
            positive: false,
            uri: path_to_uri(&main_path),
            text: main_text.clone(),
            anchor: "fn not_a_call_reference_only",
            needle: "target_probe",
        },
        Shape {
            label: "mixed_call_and_reference_caller's bare reference range (negative)",
            positive: false,
            uri: target_probe_uri.clone(),
            text: target_probe_text.clone(),
            anchor: "let _not_a_call: fn() -> u32 = target_probe;\n    value",
            needle: "target_probe",
        },
        Shape {
            label: "stringify_reference_only (negative: tokens look like a call, never executed)",
            positive: false,
            uri: target_probe_uri.clone(),
            text: target_probe_text.clone(),
            anchor: "fn stringify_reference_only",
            needle: "target_probe",
        },
        Shape {
            label: "token_swallowing_caller (negative: macro_rules! discards its input)",
            positive: false,
            uri: target_probe_uri.clone(),
            text: target_probe_text.clone(),
            anchor: "fn token_swallowing_caller",
            needle: "target_probe",
        },
        Shape {
            label: "unrelated_call_beside_reference (negative: bare reference next to an unrelated call)",
            positive: false,
            uri: target_probe_uri.clone(),
            text: target_probe_text.clone(),
            anchor: "fn unrelated_call_beside_reference",
            needle: "target_probe;",
        },
    ];

    let mut results = Vec::new();
    for shape in &shapes {
        let (start, end) = occurrence_after(&shape.text, shape.anchor, shape.needle);

        let definition = probe
            .request(
                "textDocument/definition",
                json!({
                    "textDocument": { "uri": shape.uri },
                    "position": { "line": start.0, "character": start.1 + 1 },
                }),
            )
            .expect("definition request");
        let identity_matches = definition_identity(&definition).as_ref() == Some(&target_identity);

        let probe_position = direct_argument_list_open_position(&shape.text, end);
        let (has_position, is_callable) = match probe_position {
            None => (false, false),
            Some(position) => {
                let signature = probe
                    .request(
                        "textDocument/signatureHelp",
                        json!({
                            "textDocument": { "uri": shape.uri },
                            "position": { "line": position.0, "character": position.1 },
                        }),
                    )
                    .unwrap_or(Value::Null);
                (true, signature_help_is_callable(&signature))
            }
        };

        let confirmed = identity_matches && has_position && is_callable;
        eprintln!(
            "{}: identity_matches={identity_matches} has_bounded_position={has_position} \
             signature_help_callable={is_callable} confirmed={confirmed} (expected positive={})",
            shape.label, shape.positive
        );
        results.push((shape.label, shape.positive, confirmed));
    }

    let mismatches: Vec<_> = results
        .iter()
        .filter(|(_, expected, confirmed)| expected != confirmed)
        .collect();

    eprintln!("mismatches: {mismatches:#?}");
    assert!(
        mismatches.is_empty(),
        "signatureHelp confirmation disagreed with the expected shape classification: {mismatches:#?}"
    );

    let _ = fs::remove_dir_all(&base);
}
