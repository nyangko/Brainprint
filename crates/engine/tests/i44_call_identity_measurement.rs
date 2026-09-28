//! #44 measurement gate: what the real rust-analyzer's
//! `textDocument/references` and call-hierarchy methods actually
//! return for one target Symbol, called from every shape the fixture
//! adds (same file, cross-file same-crate, cross-crate, macro-nested,
//! and a non-call reference).
//!
//! This talks to a real, freshly spawned rust-analyzer over raw LSP,
//! deliberately *not* through `RustRequest`/`RustHost::call()`: #44
//! forbids adding protocol support before this measurement proves it
//! is needed. Nothing here is product code.
//!
//! ```sh
//! cargo test -p brainprint-engine --test i44_call_identity_measurement -- --ignored --nocapture
//! ```

use std::{
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
        eprintln!("skipping #44 measurement: rust-analyzer is not installed");
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

/// No settle barrier here on purpose: this measures raw capability,
/// not the product's quiescence contract. A fixed, generous sleep is
/// the whole tool.
fn let_it_load() {
    std::thread::sleep(Duration::from_secs(8));
}

fn read_to_string(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Zero-based line/character of the first occurrence of `needle` on
/// its line, LSP's own coordinate system.
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

#[test]
#[ignore = "needs an installed rust-analyzer; spawns a real process"]
fn measure_references_and_call_hierarchy_for_the_probe_target() {
    let Some(executable) = executable_or_skip() else {
        return;
    };
    let base = env::temp_dir().join(format!("brainprint-i44-measure-{}", process::id()));
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

    let (line, character) = position_of(&target_probe_text, "pub fn target_probe");
    // Land inside the identifier, past `pub fn `.
    let character = character + "pub fn ".len() as u32 + 1;

    eprintln!("=== textDocument/references ===");
    let started = Instant::now();
    let references = probe.request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": target_probe_uri },
            "position": { "line": line, "character": character },
            "context": { "includeDeclaration": true },
        }),
    );
    eprintln!("elapsed={:?}", started.elapsed());
    eprintln!("{references:#?}");

    eprintln!("=== textDocument/prepareCallHierarchy ===");
    let started = Instant::now();
    let prepared = probe.request(
        "textDocument/prepareCallHierarchy",
        json!({
            "textDocument": { "uri": target_probe_uri },
            "position": { "line": line, "character": character },
        }),
    );
    eprintln!("elapsed={:?}", started.elapsed());
    eprintln!("{prepared:#?}");

    if let Ok(items) = &prepared {
        if let Some(item) = items.as_array().and_then(|items| items.first()) {
            eprintln!("=== callHierarchy/incomingCalls ===");
            let started = Instant::now();
            let incoming = probe.request("callHierarchy/incomingCalls", json!({ "item": item }));
            eprintln!("elapsed={:?}", started.elapsed());
            eprintln!("{incoming:#?}");
        } else {
            eprintln!("prepareCallHierarchy returned no items -- nothing to feed incomingCalls");
        }
    }

    let _ = fs::remove_dir_all(&base);
}
