//! #45 measurement gate: whether reciprocal `callHierarchy/outgoingCalls`
//! confirmation on each `incomingCalls` candidate caller can separate a
//! real call to the probe target from a plain reference, using only
//! backend identity (URI + selectionRange) -- never name-only matching,
//! never source-text heuristics.
//!
//! Talks to a real, freshly spawned rust-analyzer over raw LSP, not
//! through `RustRequest`/`RustHost::call()`: #45 forbids production
//! wiring before this measurement proves the algorithm works.
//!
//! Measured result (rust-analyzer 1.98.1, this repo's fixture, two
//! consecutive runs, deterministic both times): reciprocal
//! `outgoingCalls` correctly confirms the same-file, cross-file,
//! cross-crate, and mixed-caller shapes, and correctly rejects the
//! isolated non-call reference -- but it returns an EMPTY outgoing list
//! for the macro-nested caller (`assert_eq!(target_probe(), 7)`), so the
//! macro-nested call is silently lost, not merely unconfirmed. This is
//! the issue's own named stop condition ("loses the macro/cross-crate
//! calls"). See #45's body for the full record and the BLOCKED verdict.
//!
//! ```sh
//! cargo test -p brainprint-engine --test i45_reciprocal_outgoing_calls_measurement -- --ignored --nocapture
//! ```

use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
    process::{self, Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
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
        eprintln!("skipping #45 measurement: rust-analyzer is not installed");
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
                            "references": {},
                            "callHierarchy": { "dynamicRegistration": false },
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

/// Zero-based line/character of the first occurrence of `needle` on its
/// line, LSP's own coordinate system.
fn position_of(text: &str, needle: &str) -> (u32, u32) {
    for (line_index, line) in text.lines().enumerate() {
        if let Some(byte_at) = line.find(needle) {
            let character = line[..byte_at].encode_utf16().count();
            return (
                u32::try_from(line_index).unwrap(),
                u32::try_from(character).unwrap(),
            );
        }
    }
    panic!("{needle:?} not found");
}

/// Backend identity for a `CallHierarchyItem`: URI plus selectionRange.
/// Two items denote the same declaration iff this key matches -- never
/// name alone, and never `range` (which includes doc comments and can
/// differ by whitespace between an item found via different paths).
fn item_identity(item: &Value) -> (String, Value) {
    let uri = item["uri"].as_str().unwrap_or_default().to_string();
    (uri, item["selectionRange"].clone())
}

/// A caller's own identity, for deduping candidates before issuing one
/// `outgoingCalls` request per distinct caller.
fn caller_key(from_item: &Value) -> (String, Value) {
    item_identity(from_item)
}

#[test]
#[ignore = "needs an installed rust-analyzer; spawns a real process"]
fn reciprocal_outgoing_calls_confirms_exact_target_identity() {
    let Some(executable) = executable_or_skip() else {
        return;
    };
    let base = env::temp_dir().join(format!("brainprint-i45-measure-{}", process::id()));
    let _ = fs::remove_dir_all(&base);
    copy_tree(&fixture_source(), &base);

    let probe = Probe::start(&executable, &base);

    let target_probe_path = base.join("crates/core/src/target_probe.rs");
    let target_probe_text = read_to_string(&target_probe_path);
    let target_probe_uri = path_to_uri(&target_probe_path);
    probe.open(&target_probe_uri, &target_probe_text);

    let runner_path = base.join("crates/core/src/runner.rs");
    probe.open(&path_to_uri(&runner_path), &read_to_string(&runner_path));

    let main_path = base.join("crates/app/src/main.rs");
    probe.open(&path_to_uri(&main_path), &read_to_string(&main_path));

    let_it_load();

    let (line, character) = position_of(&target_probe_text, "pub fn target_probe");
    let character = character + "pub fn ".len() as u32 + 1;

    let prepared = probe
        .request(
            "textDocument/prepareCallHierarchy",
            json!({
                "textDocument": { "uri": target_probe_uri },
                "position": { "line": line, "character": character },
            }),
        )
        .expect("prepareCallHierarchy");
    let target_item = prepared
        .as_array()
        .and_then(|items| items.first())
        .expect("prepareCallHierarchy returned no items -- cannot measure")
        .clone();
    let target_identity = item_identity(&target_item);
    eprintln!("target identity: {target_identity:?}");

    let started = Instant::now();
    let incoming = probe
        .request(
            "callHierarchy/incomingCalls",
            json!({ "item": target_item }),
        )
        .expect("incomingCalls");
    eprintln!("incomingCalls elapsed={:?}", started.elapsed());
    let incoming_calls = incoming.as_array().cloned().unwrap_or_default();
    eprintln!("incoming candidate count: {}", incoming_calls.len());

    // Dedupe candidate callers by identity -- one outgoingCalls request
    // per distinct caller, not per incoming range.
    let mut seen = BTreeSet::new();
    let mut distinct_callers = Vec::new();
    for call in &incoming_calls {
        let from = &call["from"];
        let key = caller_key(from);
        let key_str = format!("{key:?}");
        if seen.insert(key_str) {
            distinct_callers.push(from.clone());
        }
    }
    eprintln!("distinct candidate callers: {}", distinct_callers.len());

    let mut retained_ranges: Vec<(String, Value)> = Vec::new();
    let mut outgoing_requests = 0usize;
    for caller in &distinct_callers {
        outgoing_requests += 1;
        eprintln!(
            "candidate caller: uri={} name={:?} selRange={:?}",
            caller["uri"], caller["name"], caller["selectionRange"]
        );
        let outgoing = probe
            .request("callHierarchy/outgoingCalls", json!({ "item": caller }))
            .unwrap_or_else(|error| panic!("outgoingCalls on {caller:?}: {error:?}"));
        eprintln!("  outgoing raw: {outgoing:#?}");
        let caller_uri = caller["uri"].as_str().unwrap_or_default().to_string();
        for entry in outgoing.as_array().cloned().unwrap_or_default() {
            let to = &entry["to"];
            if item_identity(to) != target_identity {
                continue;
            }
            for from_range in entry["fromRanges"].as_array().cloned().unwrap_or_default() {
                retained_ranges.push((caller_uri.clone(), from_range));
            }
        }
    }

    eprintln!("outgoingCalls requests issued: {outgoing_requests}");
    eprintln!("retained CALL ranges: {}", retained_ranges.len());
    for (uri, range) in &retained_ranges {
        eprintln!("  retained: {uri} @ {range:?}");
    }

    let retained_in = |suffix: &str| {
        retained_ranges
            .iter()
            .filter(|(uri, _)| uri.ends_with(suffix))
            .count()
    };

    let target_probe_hits = retained_in("target_probe.rs");
    let runner_hits = retained_in("runner.rs");
    let main_hits = retained_in("main.rs");

    // Diagnostic-only: this test records what the real backend does, it
    // does not gate on a hoped-for outcome. #45's body carries the
    // pass/fail verdict derived from this evidence.
    eprintln!(
        "target_probe.rs hits={target_probe_hits} (want same-file, macro-nested, \
         mixed-caller single range, aliased-import = 4)"
    );
    eprintln!("runner.rs hits={runner_hits} (want cross-file same-crate call = 1)");
    eprintln!(
        "main.rs hits={main_hits} (want exactly 1: cross-crate call, \
         not the isolated reference)"
    );
    assert!(
        !incoming_calls.is_empty(),
        "prepareCallHierarchy/incomingCalls returned no candidates at all -- \
         measurement did not run"
    );

    let _ = fs::remove_dir_all(&base);
}
