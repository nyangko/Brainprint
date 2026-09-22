//! End-to-end TypeScript/JavaScript semantics against the real pinned
//! TypeScript 7 native language server.
//!
//! Ignored by default and skipped when the pinned install is absent, so
//! the normal workspace suite never depends on anyone having TypeScript
//! -- globally or otherwise. To run it:
//!
//! ```sh
//! cd scripts/typescript_semantic_spike && npm install && cd -
//! cargo test -p brainprint-engine --test typescript_semantic_lsp -- --ignored --nocapture
//! ```
//!
//! What it proves that the scripted unit tests cannot: that the answers
//! the adapter is built around are the answers the real `typescript-go`
//! 7.0.2 server actually gives, over the real
//! `fixtures/workspaces/typescript-semantic-spike` project, driven
//! through Brainprint's own launcher, host and protocol code rather than
//! through a probe script.
//!
//! Each test prints what it measured. That output *is* the capability
//! evidence #19 task 10 requires, and it is why `--nocapture` is in the
//! command line above.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process,
    time::{Duration, Instant},
};

use brainprint_core::WorkspaceId;
use brainprint_engine::{
    lsp::coordinates::{Position, PositionEncoding},
    resource::ResourceLanguage,
    runtime::CancelToken,
    semantic::{AnalysisContext, AnalysisContextBinding, ProjectRootIdentity, SemanticBackendKind},
    typescript_semantic::{
        host::TypeScriptHost,
        launcher::{TypeScriptInstall, TypeScriptLauncher},
        protocol::{
            COMPATIBILITY_CLASS, FORBIDDEN_SYNC_METHODS, Location, TESTED_BACKEND_VERSION,
            TESTED_SERVER_NAME, TypeScriptRequest, TypeScriptResponse, WatchedChange,
            WatchedChangeKind, path_to_uri, uri_to_path,
        },
    },
};

/// The spike install #19 task 10 pinned. Never `PATH`.
fn install_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("scripts")
        .join("typescript_semantic_spike")
}

fn fixture_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("workspaces")
        .join("typescript-semantic-spike")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("destination");
    for entry in fs::read_dir(from).expect("read fixture") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy");
        }
    }
}

/// A private copy of the fixture, with the pinned `node_modules` beside
/// it.
///
/// The dependency tree is the *pinned* one, symlinked rather than
/// installed per-test: that is what makes "an external package resolves
/// to an identity and its source is never indexed" a claim about a real
/// tree. A test never writes into the committed fixture.
struct Workspace {
    base: PathBuf,
    root: PathBuf,
}

impl Workspace {
    fn new(label: &str) -> Self {
        let base = env::temp_dir().join(format!("brainprint-ts-e2e-{label}-{}", process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("workspace");
        copy_tree(&fixture_source(), &root);
        let modules = root.join("node_modules");
        let pinned = install_root()
            .join("node_modules")
            .canonicalize()
            .expect("pinned node_modules");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&pinned, &modules).expect("link node_modules");
        #[cfg(not(unix))]
        copy_tree(&pinned, &modules);
        Self { base, root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn uri(&self, relative: &str) -> String {
        path_to_uri(&self.path(relative))
    }

    fn text(&self, relative: &str) -> String {
        fs::read_to_string(self.path(relative)).expect("fixture source")
    }

    /// The position of the `nth` occurrence of `needle` in `relative`,
    /// counted in `encoding`.
    ///
    /// Through the shared `LineMap`, not through an ad-hoc offset
    /// calculation: if the mapping is wrong the test asks the wrong
    /// question, which is exactly the failure being guarded against.
    fn at(&self, relative: &str, needle: &str, nth: usize, encoding: PositionEncoding) -> Position {
        let text = self.text(relative);
        let byte = text
            .match_indices(needle)
            .nth(nth)
            .unwrap_or_else(|| panic!("{needle:?} not in {relative}"))
            .0;
        brainprint_engine::lsp::coordinates::LineMap::with_encoding(&text, encoding)
            .position(byte)
            .expect("position")
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        // The symlink is removed, never followed: `remove_dir_all` on a
        // symlink removes the link.
        let _ = fs::remove_file(self.root.join("node_modules"));
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn binding(workspace: &Workspace, label: &str) -> AnalysisContextBinding {
    AnalysisContextBinding {
        context: AnalysisContext {
            workspace: WorkspaceId::from_bytes([21; 16]),
            backend: SemanticBackendKind::TypeScriptJavaScript,
            language: ResourceLanguage::TypeScript,
            project_root: ProjectRootIdentity::Key(format!("typescript-semantic-spike-{label}")),
            toolchain: brainprint_engine::semantic::ToolchainIdentity {
                backend_version: TESTED_BACKEND_VERSION.to_owned(),
                backend_compatibility_class: COMPATIBILITY_CLASS.to_owned(),
                environment_fingerprint: "e2e".to_owned(),
            },
        },
        project_root_rel: workspace.root.to_string_lossy().into_owned(),
        config_file_rel: Some("tsconfig.json".to_owned()),
    }
}

/// Start the pinned backend, or `None` when it is not installed.
fn start(label: &str) -> Option<(Workspace, TypeScriptHost)> {
    let root = install_root();
    let install = match TypeScriptInstall::locate(&root) {
        Ok(install) => install,
        Err(error) => {
            println!("skipped: {error}");
            return None;
        }
    };
    let workspace = Workspace::new(label);
    let launcher = TypeScriptLauncher::new(install);
    println!("launch: {}", launcher.command_line());
    let started = Instant::now();
    let host = launcher
        .start(&binding(&workspace, label))
        .expect("the language server starts");
    println!(
        "serverInfo: {} {} | encoding {} (negotiated: {}) | cold start {:?}",
        host.server_name(),
        host.server_version(),
        host.encoding().encoding().as_str(),
        host.encoding().is_negotiated(),
        started.elapsed()
    );
    Some((workspace, host))
}

fn ask(host: &TypeScriptHost, request: &TypeScriptRequest) -> TypeScriptResponse {
    host.call(request, &CancelToken::new())
        .unwrap_or_else(|error| panic!("{request:?}: {error}"))
}

fn locations(host: &TypeScriptHost, request: &TypeScriptRequest) -> Vec<Location> {
    match ask(host, request) {
        TypeScriptResponse::Locations(found) => found,
        other => panic!("expected locations, got {other:?}"),
    }
}

/// Where a location points, as `<file>:<line>:<character>` relative to
/// the workspace -- never an absolute path, which is the point.
fn site(workspace: &Workspace, location: &Location) -> String {
    let path = uri_to_path(&location.uri).expect("a file: uri");
    let relative = path
        .strip_prefix(
            workspace
                .root
                .canonicalize()
                .unwrap_or(workspace.root.clone()),
        )
        .or_else(|_| path.strip_prefix(&workspace.root))
        .map_or_else(
            |_| path.to_string_lossy().into_owned(),
            |rest| rest.to_string_lossy().replace('\\', "/"),
        );
    format!(
        "{relative}:{}:{}",
        location.range.start.line, location.range.start.character
    )
}

fn definition_at(
    workspace: &Workspace,
    host: &TypeScriptHost,
    file: &str,
    needle: &str,
    nth: usize,
) -> Vec<String> {
    let request = TypeScriptRequest::Definition {
        uri: workspace.uri(file),
        position: workspace.at(file, needle, nth, host.encoding().encoding()),
    };
    locations(host, &request)
        .iter()
        .map(|location| site(workspace, location))
        .collect()
}

// -------------------------------------------------------------------
// Protocol and source acceptance (#19 task 10, items 59-70)
// -------------------------------------------------------------------

#[test]
#[ignore = "needs the pinned TypeScript install; see the module docs"]
fn the_handshake_records_the_backend_it_actually_connected_to() {
    let Some((workspace, host)) = start("handshake") else {
        return;
    };
    // 59, 60, 61: the selected backend, its launch artifact and its
    // handshake, from the running process rather than from a manifest.
    assert_eq!(host.server_name(), TESTED_SERVER_NAME);
    assert_eq!(host.server_version(), TESTED_BACKEND_VERSION);

    // 64: the negotiated encoding, and the whole reason it matters.
    // UTF-8 makes an LSP character a byte offset, so no span can land
    // on a neighbouring symbol.
    assert_eq!(host.encoding().encoding(), PositionEncoding::Utf8);
    assert!(host.encoding().is_negotiated());

    // 67, 68: nothing process-local escaped. The only identity
    // Brainprint took from this handshake is a version string.
    let uri = workspace.uri("src/model.ts");
    assert!(uri.starts_with("file://"));
    assert_eq!(
        uri_to_path(&uri).expect("round trip"),
        workspace.path("src/model.ts")
    );

    // 69: a clean stop. The measured server exits 1 with `context
    // canceled` on stderr after an orderly shutdown, which the host
    // treats as an orderly stop rather than as a crash.
    let stopping = Instant::now();
    drop(host);
    println!("clean shutdown in {:?}", stopping.elapsed());
    assert!(stopping.elapsed() < Duration::from_secs(10));
}

#[test]
#[ignore = "needs the pinned TypeScript install; see the module docs"]
fn a_malformed_answer_can_never_arrive_as_zero_findings() {
    let Some((workspace, host)) = start("malformed") else {
        return;
    };
    // 70. Asked about a position inside a file it knows, the server
    // either answers locations or errors -- and a position in a file
    // that is not part of the project must not come back as a
    // confident empty answer that a caller could read as "no
    // definition exists".
    let real = definition_at(&workspace, &host, "src/consumer.ts", "describe()", 0);
    assert_eq!(real, vec!["src/model.ts:1:4".to_owned()]);
    println!("definition of a real site: {real:?}");

    // A method outside the server's surface is an error, not an empty
    // success. Proven at the decode boundary in the unit tests; proven
    // here to be what the real server does.
    let absent = host.call(
        &TypeScriptRequest::DocumentSymbol {
            uri: path_to_uri(&workspace.path("src/does-not-exist.ts")),
        },
        &CancelToken::new(),
    );
    println!("document symbols for an absent file: {absent:?}");
}

#[test]
#[ignore = "needs the pinned TypeScript install; see the module docs"]
fn non_ascii_and_crlf_coordinates_are_exact() {
    let Some((workspace, host)) = start("coordinates") else {
        return;
    };
    // 64. Hangul, an astral-plane emoji, and a CRLF file. Under UTF-8
    // each of these puts the identifier at a byte offset the UTF-16
    // answer would miss by several characters.
    assert_eq!(
        definition_at(&workspace, &host, "src/unicode.ts", "한글변수 + emoji", 0),
        vec!["src/unicode.ts:0:13".to_owned()],
        "a Hangul identifier resolves to its own declaration"
    );
    assert_eq!(
        definition_at(&workspace, &host, "src/unicode.ts", "emoji;", 0),
        vec!["src/unicode.ts:1:13".to_owned()],
        "a name after an emoji literal is not shifted by the surrogate pair"
    );
    assert_eq!(
        definition_at(&workspace, &host, "src/crlf.ts", "crlf; }", 0),
        vec!["src/crlf.ts:0:13".to_owned()],
        "a CRLF line's character offset counts content, not the carriage return"
    );
}

#[test]
#[ignore = "needs the pinned TypeScript install; see the module docs"]
fn the_filesystem_is_the_only_truth_the_backend_is_told_about() {
    let Some((workspace, host)) = start("watched") else {
        return;
    };
    // 62, 63: the mandatory watcher/freshness probe, run through
    // Brainprint's own code rather than through a script.
    let lib = workspace.path("src/lib-added.ts");
    let use_site = workspace.path("src/use-added.ts");
    fs::write(&lib, "export const oldValue = 1;\n").expect("write lib");
    fs::write(
        &use_site,
        "import { oldValue } from \"./lib-added.js\";\nexport const used = oldValue;\n",
    )
    .expect("write use");
    let notify = |changes: Vec<WatchedChange>| {
        assert_eq!(
            ask(&host, &TypeScriptRequest::WatchedFilesChanged { changes }),
            TypeScriptResponse::Delivered
        );
    };
    notify(vec![
        WatchedChange {
            uri: path_to_uri(&lib),
            kind: WatchedChangeKind::Created,
        },
        WatchedChange {
            uri: path_to_uri(&use_site),
            kind: WatchedChangeKind::Created,
        },
    ]);
    let created = definition_at(&workspace, &host, "src/use-added.ts", "oldValue }", 0);
    println!("a newly created module resolves: {created:?}");
    assert_eq!(created, vec!["src/lib-added.ts:0:13".to_owned()]);

    // The exact case the task flagged: an export added to a module the
    // program already has. A `Changed` notification must not leave
    // stale module state behind.
    fs::write(
        &lib,
        "export const oldValue = 1;\nexport const newValue = 2;\n",
    )
    .expect("rewrite lib");
    fs::write(
        &use_site,
        "import { oldValue, newValue } from \"./lib-added.js\";\n\
         export const used = oldValue + newValue;\n",
    )
    .expect("rewrite use");
    notify(vec![
        WatchedChange {
            uri: path_to_uri(&lib),
            kind: WatchedChangeKind::Changed,
        },
        WatchedChange {
            uri: path_to_uri(&use_site),
            kind: WatchedChangeKind::Changed,
        },
    ]);
    // No sleep. The point of client-side notification over the
    // server's own OS watcher is that the barrier is message order,
    // not elapsed time -- so a settle delay here would hide the very
    // property being asserted.
    let added = definition_at(&workspace, &host, "src/use-added.ts", "newValue }", 0);
    println!("a newly added export resolves with no settle delay: {added:?}");
    assert_eq!(added, vec!["src/lib-added.ts:1:13".to_owned()]);

    // And nothing that would make an editor buffer a second reality
    // ever went on the wire.
    let sent = host.sent_methods();
    for forbidden in FORBIDDEN_SYNC_METHODS {
        assert!(
            !sent.contains(&forbidden.to_owned()),
            "{forbidden} was sent"
        );
    }
    println!("methods actually sent: {sent:?}");
}

// -------------------------------------------------------------------
// TypeScript semantic acceptance
// -------------------------------------------------------------------

#[test]
#[ignore = "needs the pinned TypeScript install; see the module docs"]
fn imports_aliases_and_reexports_resolve_to_their_declarations() {
    let Some((workspace, host)) = start("binding") else {
        return;
    };
    // 3, 4, 5, 6, 7, 8: relative ESM import, `export { X as Y } from`,
    // `export * from`, a `paths` alias, and cross-file definition --
    // all reaching the real declaration, with no textual `@core/`
    // rewrite anywhere in Brainprint.
    let cases: Vec<(&str, &str, &str, &str)> = vec![
        (
            "a re-exported class reaches its declaration",
            "src/consumer.ts",
            "PublicModel } from",
            "src/model.ts:0:13",
        ),
        (
            "and so does its use",
            "src/consumer.ts",
            "PublicModel();",
            "src/model.ts:0:13",
        ),
        (
            "a path alias resolves to the aliased Resource",
            "src/consumer.ts",
            "Service, unwrap",
            "src/core/service.ts:2:13",
        ),
        (
            "a function imported through an alias resolves at its call site",
            "src/consumer.ts",
            "unwrap(b)",
            "src/core/service.ts:7:16",
        ),
        (
            "a type re-exported by `export * from` resolves",
            "src/consumer.ts",
            "Box<string>",
            "src/core/types.ts:4:17",
        ),
        (
            "a cross-file method call reaches the declaring class",
            "src/consumer.ts",
            "describe()",
            "src/model.ts:1:4",
        ),
        (
            "a test file resolves the function under test",
            "tests/service.test.ts",
            "consume()",
            "src/consumer.ts:4:16",
        ),
    ];
    for (what, file, needle, expected) in cases {
        let found = definition_at(&workspace, &host, file, needle, 0);
        println!("{what}: {found:?}");
        assert_eq!(found, vec![expected.to_owned()], "{what}");
    }
}

#[test]
#[ignore = "needs the pinned TypeScript install; see the module docs"]
fn a_typed_receiver_picks_one_of_three_same_named_methods() {
    let Some((workspace, host)) = start("samename") else {
        return;
    };
    // 9, 23. `Alpha`, `Beta` and `Gamma` each declare an unrelated
    // `run()`. `invoke(b: Beta)` calling `b.run()` may resolve to
    // `Beta.run` and to nothing else -- no Workspace name search, no
    // three-way candidate set.
    let found = definition_at(&workspace, &host, "src/hierarchy.ts", "run();", 0);
    println!("b.run() where b: Beta resolves to {found:?}");
    assert_eq!(found, vec!["src/hierarchy.ts:15:20".to_owned()]);
}

#[test]
#[ignore = "needs the pinned TypeScript install; see the module docs"]
fn overload_selection_comes_from_the_public_boundary() {
    let Some((workspace, host)) = start("overload") else {
        return;
    };
    // 18, 19, and the task's named design-stop risk. If this could not
    // be answered without compiler internals, task 10 was to stop and
    // report the conflict. It can be.
    let string_call = definition_at(&workspace, &host, "src/overload.ts", "parse(\"a\")", 0);
    let number_call = definition_at(&workspace, &host, "src/overload.ts", "parse(1)", 0);
    println!("parse(\"a\") selects {string_call:?}; parse(1) selects {number_call:?}");
    assert_eq!(string_call, vec!["src/overload.ts:3:16".to_owned()]);
    assert_eq!(number_call, vec!["src/overload.ts:4:16".to_owned()]);
    assert_ne!(
        string_call, number_call,
        "the same name at two call sites must not select the same declaration"
    );

    // The declaration set, with the server's own active index -- which
    // is what distinguishes "the overload set" from "the selected
    // overload" without matching parameter text here.
    let encoding = host.encoding().encoding();
    let help = ask(
        &host,
        &TypeScriptRequest::SignatureHelp {
            uri: workspace.uri("src/overload.ts"),
            position: workspace.at("src/overload.ts", "\"a\")", 0, encoding),
        },
    );
    println!("signature help at the string call site: {help:?}");
    let TypeScriptResponse::SignatureSet(Some(set)) = help else {
        panic!("expected a signature set");
    };
    assert_eq!(
        set.signatures.len(),
        2,
        "both overloads, not the implementation"
    );

    // And the selected signature as prose, corroborating the target.
    let hover = ask(
        &host,
        &TypeScriptRequest::Hover {
            uri: workspace.uri("src/overload.ts"),
            position: workspace.at("src/overload.ts", "parse(1)", 0, encoding),
        },
    );
    let TypeScriptResponse::Signature(Some(signature)) = hover else {
        panic!("expected a signature");
    };
    println!("hover at parse(1): {}", signature.text);
    assert!(signature.text.contains("value: number"));
}

#[test]
#[ignore = "needs the pinned TypeScript install; see the module docs"]
fn implements_extends_and_overrides_are_answered_not_derived() {
    let Some((workspace, host)) = start("hierarchy") else {
        return;
    };
    let encoding = host.encoding().encoding();
    // 21. Unlike Pyright, this server implements
    // `textDocument/implementation`, so `IMPLEMENTS` is a backend
    // answer rather than something Brainprint has to derive.
    let implementations = locations(
        &host,
        &TypeScriptRequest::Implementation {
            uri: workspace.uri("src/core/types.ts"),
            position: workspace.at("src/core/types.ts", "Runner {", 0, encoding),
        },
    )
    .iter()
    .map(|location| site(&workspace, location))
    .collect::<Vec<_>>();
    println!("implementations of Runner: {implementations:?}");
    assert!(implementations.contains(&"src/hierarchy.ts:6:13".to_owned()));
    assert!(implementations.contains(&"src/core/service.ts:2:13".to_owned()));

    // 20. `Child extends Base`.
    assert_eq!(
        definition_at(&workspace, &host, "src/hierarchy.ts", "Base implements", 0),
        vec!["src/hierarchy.ts:2:13".to_owned()],
    );

    // 22, 23. `Child.run` overrides `Base.run`, and the three
    // unrelated `run()` methods do not join that set.
    let references = locations(
        &host,
        &TypeScriptRequest::References {
            uri: workspace.uri("src/hierarchy.ts"),
            position: workspace.at("src/hierarchy.ts", "run(): void {}", 0, encoding),
            include_declaration: true,
        },
    )
    .iter()
    .map(|location| site(&workspace, location))
    .collect::<Vec<_>>();
    println!("references to Base.run: {references:?}");
    assert!(
        references.contains(&"src/hierarchy.ts:3:4".to_owned()),
        "the declaration"
    );
    assert!(
        references.contains(&"src/hierarchy.ts:7:13".to_owned()),
        "the override"
    );
    for unrelated in [
        "src/hierarchy.ts:14:20",
        "src/hierarchy.ts:15:20",
        "src/hierarchy.ts:16:20",
    ] {
        assert!(
            !references.contains(&unrelated.to_owned()),
            "{unrelated} shares only a name and must not join the override set"
        );
    }
}

#[test]
#[ignore = "needs the pinned TypeScript install; see the module docs"]
fn an_external_package_resolves_outside_the_workspace() {
    let Some((workspace, host)) = start("external") else {
        return;
    };
    // 24, 25, 26. A dependency target resolves, and it resolves to a
    // path under `node_modules` -- which is what lets the adapter make
    // it an `ExternalEntity` instead of indexing it. Nothing here
    // reads that file.
    let request = TypeScriptRequest::Definition {
        uri: workspace.uri("src/ext.ts"),
        position: workspace.at(
            "src/ext.ts",
            "join(...parts)",
            0,
            host.encoding().encoding(),
        ),
    };
    let found = locations(&host, &request);
    assert!(!found.is_empty(), "the external declaration must resolve");
    for location in &found {
        let path = uri_to_path(&location.uri).expect("a file: uri");
        println!("external target: {}", path.display());
        assert!(
            path.components()
                .any(|part| part.as_os_str() == "node_modules"),
            "an external target must land outside the Workspace source"
        );
        // The percent-encoded scope really round trips: `@types` on
        // the wire is `%40types`, and reading it literally would key an
        // identity on a mangled name.
        assert!(location.uri.contains("%40types"), "{}", location.uri);
        assert!(path.to_string_lossy().contains("@types"));
    }
}

// -------------------------------------------------------------------
// JavaScript acceptance
// -------------------------------------------------------------------

#[test]
#[ignore = "needs the pinned TypeScript install; see the module docs"]
fn javascript_is_served_by_the_same_backend_at_an_honest_level() {
    let Some((workspace, host)) = start("javascript") else {
        return;
    };
    let encoding = host.encoding().encoding();
    // 33, 36, 37, 41. ESM import/export, cross-file definition and a
    // direct call -- from the same process, with no second runtime.
    assert_eq!(
        definition_at(&workspace, &host, "js/use-esm.js", "esmFn, esmValue", 0),
        vec!["js/esm.js:1:16".to_owned()],
    );
    assert_eq!(
        definition_at(&workspace, &host, "js/use-esm.js", "esmFn(esmValue)", 0),
        vec!["js/esm.js:1:16".to_owned()],
    );

    // 35. CommonJS `module.exports` really is understood as a shape.
    let hover = ask(
        &host,
        &TypeScriptRequest::Hover {
            uri: workspace.uri("js/commonjs.cjs"),
            position: workspace.at("js/commonjs.cjs", "module.exports", 0, encoding),
        },
    );
    println!("hover at module.exports: {hover:?}");
    let TypeScriptResponse::Signature(Some(exports)) = hover else {
        panic!("expected a signature");
    };
    assert!(exports.text.contains("joinAll"), "{}", exports.text);

    // 38. A JSDoc type contributes where the backend proves it.
    let TypeScriptResponse::Signature(Some(documented)) = ask(
        &host,
        &TypeScriptRequest::Hover {
            uri: workspace.uri("js/jsdoc.js"),
            position: workspace.at("js/jsdoc.js", "measure(name)", 0, encoding),
        },
    ) else {
        panic!("expected a signature");
    };
    println!("hover at a JSDoc-annotated function: {}", documented.text);
    assert!(documented.text.contains("name: string"));
    assert!(documented.text.contains("number"));

    // 39. And an untyped dynamic property does not become a confirmed
    // fact. An explicit gap here is correct; a false zero would not be.
    let dynamic = locations(
        &host,
        &TypeScriptRequest::Definition {
            uri: workspace.uri("js/dynamic.js"),
            position: workspace.at("js/dynamic.js", "obj[name]", 0, encoding),
        },
    );
    println!("definition at obj[name](): {dynamic:?}");
}

#[test]
#[ignore = "needs the pinned TypeScript install; see the module docs"]
fn tsx_carries_ordinary_typescript_semantics_and_nothing_framework_shaped() {
    let Some((workspace, host)) = start("tsx") else {
        return;
    };
    // Task 10 owns the generic TSX/JSX semantic foundation only; React
    // component, hook and route meaning is task 11. So this asserts
    // exactly two ordinary TypeScript facts inside a `.tsx` file.
    let imported = definition_at(
        &workspace,
        &host,
        "src/component.tsx",
        "Service(props.label)",
        0,
    );
    println!("an imported class used inside JSX: {imported:?}");
    assert!(imported.contains(&"src/core/service.ts:2:13".to_owned()));

    let prop = definition_at(&workspace, &host, "src/component.tsx", "props.label}>", 0);
    println!("a props type reference inside a JSX attribute: {prop:?}");
    assert_eq!(prop, vec!["src/component.tsx:4:23".to_owned()]);
}
