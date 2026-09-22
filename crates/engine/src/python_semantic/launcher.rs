//! Finding and starting the pinned Pyright type server.
//!
//! The artifact is always a Brainprint-managed or project-local
//! install: a directory containing `node_modules/pyright-typeserver`.
//! Nothing here searches `PATH` for a Pyright, and a missing install is
//! a typed failure rather than a fallback. A developer who happens to
//! have some other Pyright on their machine must not silently change
//! what Brainprint publishes, and a machine with none must still get
//! every structural answer I2/I3 already has.

use std::{
    error::Error,
    fmt, fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, atomic::AtomicU64},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use super::{
    host::{ClientHandler, PyrightHost, PythonSettings},
    jsonrpc::Client,
    protocol::{
        COMPATIBILITY_CLASS, ProtocolCompatibility, PythonRequest, PythonResponse, method,
        path_to_uri,
    },
};
use crate::{
    runtime::{CancelToken, HostError, SemanticBackendLauncher, SemanticRuntimeHost},
    semantic::{AnalysisContextBinding, SemanticBackendKind},
};

/// The npm package that carries the type server.
pub const PACKAGE: &str = "pyright-typeserver";

/// When a freshly started type server may be called READY.
///
/// A backend that has answered `initialize` is not one that can answer
/// questions yet. Measured on the task 5 fixture: for the first ~1.6 s
/// after the handshake, `typeServer/getPythonSearchPaths` answers
/// `null` and `typeServer/getDeclaredType` answers a type of kind
/// `Unknown` -- not an error, just "not analyzed". The TSP requests do
/// not bind a file on demand the way the LSP handlers do. Publishing
/// out of that window would record real UNRESOLVED answers that only
/// mean "too early", which is the false zero this tier exists to
/// prevent.
///
/// There is no readiness notification in the protocol, so readiness is
/// probed rather than timed: the backend is ready when it answers a
/// TSP question about the project. That is a positive signal, unlike a
/// sleep, and unlike snapshot quiescence it cannot be satisfied by a
/// backend that has not started analyzing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Readiness {
    pub poll: Duration,
    /// How long to keep probing before starting anyway. Exceeding it is
    /// not an error: the queries will report what they can, and a
    /// capability that answers nothing is reported as unresolved rather
    /// than as a failure.
    pub timeout: Duration,
}

impl Default for Readiness {
    fn default() -> Self {
        Self {
            poll: Duration::from_millis(50),
            timeout: Duration::from_secs(60),
        }
    }
}

/// Why the pinned install could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// No `node_modules/pyright-typeserver` under the configured root.
    PackageMissing {
        root: PathBuf,
    },
    /// The package is there but its entry point is not.
    ScriptMissing {
        path: PathBuf,
    },
    ManifestUnreadable {
        path: PathBuf,
        detail: String,
    },
    /// The manifest carries no version, so nothing could be recorded as
    /// the backend version a publication was produced under.
    VersionMissing {
        path: PathBuf,
    },
}

impl fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PackageMissing { root } => write!(
                formatter,
                "no {PACKAGE} install under {}; Python semantics are unavailable",
                root.display()
            ),
            Self::ScriptMissing { path } => {
                write!(
                    formatter,
                    "{PACKAGE} entry point missing at {}",
                    path.display()
                )
            }
            Self::ManifestUnreadable { path, detail } => write!(
                formatter,
                "unreadable {PACKAGE} manifest at {}: {detail}",
                path.display()
            ),
            Self::VersionMissing { path } => write!(
                formatter,
                "{PACKAGE} manifest at {} states no version",
                path.display()
            ),
        }
    }
}

impl Error for InstallError {}

/// A located, version-pinned type server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PyrightInstall {
    /// The Node runtime to run it with. Explicit rather than
    /// discovered, so nothing about which backend runs is implicit.
    pub node: PathBuf,
    /// `<root>/node_modules/pyright-typeserver/pyright-typeserver.js`.
    pub server_script: PathBuf,
    /// The package version, straight from the manifest. This is what
    /// task 3 records as the backend version.
    pub package_version: String,
}

impl PyrightInstall {
    /// Locate the type server under `root`, which must be a directory
    /// Brainprint or the project controls.
    ///
    /// `node` is the Node runtime. It is a parameter and not a `PATH`
    /// lookup so that the choice is always someone's decision.
    pub fn locate(root: &Path, node: impl Into<PathBuf>) -> Result<Self, InstallError> {
        let package = root.join("node_modules").join(PACKAGE);
        if !package.is_dir() {
            return Err(InstallError::PackageMissing {
                root: root.to_path_buf(),
            });
        }
        let server_script = package.join("pyright-typeserver.js");
        if !server_script.is_file() {
            return Err(InstallError::ScriptMissing {
                path: server_script,
            });
        }
        let manifest = package.join("package.json");
        let text =
            fs::read_to_string(&manifest).map_err(|error| InstallError::ManifestUnreadable {
                path: manifest.clone(),
                detail: error.to_string(),
            })?;
        let parsed: Value =
            serde_json::from_str(&text).map_err(|error| InstallError::ManifestUnreadable {
                path: manifest.clone(),
                detail: error.to_string(),
            })?;
        let package_version = parsed
            .get("version")
            .and_then(Value::as_str)
            .filter(|version| !version.is_empty())
            .ok_or(InstallError::VersionMissing { path: manifest })?
            .to_owned();
        Ok(Self {
            node: node.into(),
            server_script,
            package_version,
        })
    }
}

/// Starts Python semantic runtimes for one Workspace.
///
/// Registered once with the task 2 supervisor, which owns every
/// process: the launcher never keeps a handle, so no Agent or session
/// can end up owning a backend.
pub struct PythonLauncher {
    install: PyrightInstall,
    /// Where the Workspace lives now. A locator, deliberately not part
    /// of any identity.
    workspace_root: PathBuf,
    settings: PythonSettings,
    readiness: Readiness,
}

impl PythonLauncher {
    #[must_use]
    pub fn new(
        install: PyrightInstall,
        workspace_root: impl Into<PathBuf>,
        settings: PythonSettings,
    ) -> Self {
        Self {
            install,
            workspace_root: workspace_root.into(),
            settings,
            readiness: Readiness::default(),
        }
    }

    #[must_use]
    pub const fn with_readiness(mut self, readiness: Readiness) -> Self {
        self.readiness = readiness;
        self
    }

    #[must_use]
    pub const fn install(&self) -> &PyrightInstall {
        &self.install
    }

    /// The compatibility class this launcher will accept, for building
    /// a [`ToolchainIdentity`](crate::semantic::ToolchainIdentity)
    /// before anything is started.
    #[must_use]
    pub const fn compatibility_class() -> &'static str {
        COMPATIBILITY_CLASS
    }

    fn project_root(&self, binding: &AnalysisContextBinding) -> PathBuf {
        if binding.project_root_rel.is_empty() || binding.project_root_rel == "." {
            self.workspace_root.clone()
        } else {
            self.workspace_root.join(&binding.project_root_rel)
        }
    }

    /// Start a child and complete the LSP handshake and TSP version
    /// check.
    pub fn start(&self, binding: &AnalysisContextBinding) -> Result<PyrightHost, HostError> {
        let root = self.project_root(binding);
        let mut child = spawn(&self.install, &root)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HostError::new("the type server has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HostError::new("the type server has no stdout"))?;
        // Drained and dropped. Backend diagnostics are not semantic
        // truth, and an unread pipe eventually blocks the child.
        if let Some(mut stderr) = child.stderr.take() {
            thread::spawn(move || {
                let mut sink = [0_u8; 4096];
                while matches!(stderr.read(&mut sink), Ok(read) if read > 0) {}
            });
        }

        let snapshot_changes = Arc::new(AtomicU64::new(0));
        let handler = Arc::new(ClientHandler::new(
            self.settings.clone(),
            Arc::clone(&snapshot_changes),
        ));
        let client = Client::new(Box::new(stdout), Box::new(stdin), handler);

        match handshake(&client, &root) {
            Ok((version, compatibility)) => {
                await_ready(&client, &path_to_uri(&root), self.readiness);
                Ok(PyrightHost::new(
                    client,
                    Some(child),
                    version,
                    compatibility,
                    snapshot_changes,
                ))
            }
            Err(failure) => {
                client.close();
                let _ = child.kill();
                let _ = child.wait();
                Err(failure)
            }
        }
    }
}

impl SemanticBackendLauncher for PythonLauncher {
    fn kind(&self) -> SemanticBackendKind {
        SemanticBackendKind::Python
    }

    fn launch(
        &self,
        binding: &AnalysisContextBinding,
    ) -> Result<Arc<dyn SemanticRuntimeHost>, HostError> {
        Ok(Arc::new(self.start(binding)?))
    }
}

fn spawn(install: &PyrightInstall, cwd: &Path) -> Result<Child, HostError> {
    Command::new(&install.node)
        .arg(&install.server_script)
        .arg("--stdio")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            HostError::new(format!(
                "could not start {} {}: {error}",
                install.node.display(),
                install.server_script.display()
            ))
        })
}

/// `initialize`, `initialized`, then the TSP version check.
pub(crate) fn handshake(
    client: &Client,
    root: &Path,
) -> Result<(String, ProtocolCompatibility), HostError> {
    let cancel = CancelToken::new();
    let root_uri = path_to_uri(root);
    client
        .request(
            method::INITIALIZE,
            Some(initialize_params(&root_uri)),
            &cancel,
        )
        .map_err(|failure| HostError::new(format!("initialize failed: {failure}")))?;
    client
        .notify(method::INITIALIZED, Some(json!({})))
        .map_err(|failure| HostError::new(format!("initialized failed: {failure}")))?;

    let request = PythonRequest::ProtocolVersion;
    let (wire, params) = request.wire();
    let answered = client
        .request(wire, params, &cancel)
        .map_err(|failure| HostError::new(format!("{wire} failed: {failure}")))?;
    let PythonResponse::ProtocolVersion(version) = request
        .decode(&answered)
        .map_err(|error| HostError::new(error.to_string()))?
    else {
        return Err(HostError::new(
            "the type server reported no protocol version",
        ));
    };

    let compatibility = ProtocolCompatibility::classify(&version);
    if !compatibility.usable() {
        // TSP is pre-1.0 and an unknown minor may have changed a shape
        // without saying so. Refusing to start is the safe degradation:
        // structural truth stands and no semantic fact is published
        // from a protocol nobody has read.
        return Err(HostError::new(format!(
            "type server protocol {version} is outside the tested {COMPATIBILITY_CLASS}; \
             Python semantics degrade rather than parse it"
        )));
    }
    Ok((version, compatibility))
}

/// Probe until the type server answers a TSP question about the
/// project, or the budget runs out. See [`Readiness`].
///
/// The budget is enforced with a cancel token rather than by checking
/// the clock between probes: a backend that accepts a request and
/// never answers would otherwise block the launch forever, and a
/// launch that never returns is worse than a backend that starts
/// unready.
fn await_ready(client: &Client, root_uri: &str, readiness: Readiness) {
    let deadline = Instant::now() + readiness.timeout;
    let cancel = CancelToken::new();
    let watchdog = cancel.clone();
    let timeout = readiness.timeout;
    thread::spawn(move || {
        thread::sleep(timeout);
        watchdog.cancel();
    });
    while Instant::now() < deadline && !cancel.is_cancelled() {
        if answers_about_the_project(client, root_uri, &cancel) {
            return;
        }
        thread::sleep(readiness.poll);
    }
}

fn answers_about_the_project(client: &Client, root_uri: &str, cancel: &CancelToken) -> bool {
    let snapshot = match ask(client, &PythonRequest::Snapshot, cancel) {
        Some(PythonResponse::Snapshot(snapshot)) => snapshot,
        _ => return false,
    };
    matches!(
        ask(
            client,
            &PythonRequest::SearchPaths {
                from_uri: root_uri.to_owned(),
                snapshot,
            },
            cancel,
        ),
        Some(PythonResponse::SearchPaths(paths)) if !paths.is_empty()
    )
}

/// One typed question, with every failure folded into `None`: this is a
/// readiness probe, and a backend that is not ready yet is not a
/// backend that failed.
fn ask(client: &Client, request: &PythonRequest, cancel: &CancelToken) -> Option<PythonResponse> {
    let (wire, params) = request.wire();
    let result = client.request(wire, params, cancel).ok()?;
    request.decode(&result).ok()
}

fn initialize_params(root_uri: &str) -> Value {
    json!({
        "processId": std::process::id(),
        "rootUri": root_uri,
        "workspaceFolders": [{ "uri": root_uri, "name": "workspace" }],
        "capabilities": {
            "general": { "positionEncodings": ["utf-16"] },
            "workspace": {
                "configuration": true,
                "workspaceFolders": true,
                "didChangeWatchedFiles": { "dynamicRegistration": true },
            },
            "textDocument": {
                "definition": {},
                "declaration": {},
                "typeDefinition": {},
                "references": {},
                "callHierarchy": {},
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn install_tree(root: &Path, version: Option<&str>) {
        let package = root.join("node_modules").join(PACKAGE);
        fs::create_dir_all(&package).expect("package dir");
        fs::write(package.join("pyright-typeserver.js"), "// stub\n").expect("script");
        let manifest = match version {
            Some(version) => format!("{{\"name\":\"{PACKAGE}\",\"version\":\"{version}\"}}"),
            None => format!("{{\"name\":\"{PACKAGE}\"}}"),
        };
        fs::write(package.join("package.json"), manifest).expect("manifest");
    }

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("brainprint-python-launcher-{name}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("scratch");
        root
    }

    #[test]
    fn a_pinned_local_install_is_located_with_its_version() {
        let root = scratch("pinned");
        install_tree(&root, Some("1.1.414"));

        let install = PyrightInstall::locate(&root, "node").expect("located");
        assert_eq!(install.package_version, "1.1.414");
        assert_eq!(
            install.server_script,
            root.join("node_modules")
                .join(PACKAGE)
                .join("pyright-typeserver.js")
        );
        assert!(
            install.server_script.starts_with(&root),
            "the artifact comes from the configured root, not from PATH"
        );
    }

    #[test]
    fn a_missing_install_is_a_typed_failure_and_never_a_global_fallback() {
        let root = scratch("missing");
        let error = PyrightInstall::locate(&root, "node").expect_err("no install");
        assert_eq!(error, InstallError::PackageMissing { root: root.clone() });
        // Even with a same-named binary on PATH, nothing is resolved:
        // the only place looked at is the configured root.
        assert!(error.to_string().contains(&root.display().to_string()));
    }

    #[test]
    fn a_broken_install_is_reported_rather_than_worked_around() {
        let root = scratch("broken");
        let package = root.join("node_modules").join(PACKAGE);
        fs::create_dir_all(&package).expect("package dir");
        assert!(matches!(
            PyrightInstall::locate(&root, "node"),
            Err(InstallError::ScriptMissing { .. })
        ));

        fs::write(package.join("pyright-typeserver.js"), "// stub\n").expect("script");
        assert!(matches!(
            PyrightInstall::locate(&root, "node"),
            Err(InstallError::ManifestUnreadable { .. })
        ));

        install_tree(&root, None);
        assert!(matches!(
            PyrightInstall::locate(&root, "node"),
            Err(InstallError::VersionMissing { .. })
        ));
    }

    #[test]
    fn launching_without_an_install_fails_instead_of_starting_something_else() {
        let root = scratch("nostart");
        install_tree(&root, Some("1.1.414"));
        let mut install = PyrightInstall::locate(&root, "node").expect("located");
        // A Node that cannot exist, so the spawn itself must fail.
        install.node = root.join("definitely-not-a-runtime");

        let launcher = PythonLauncher::new(install, &root, PythonSettings::default());
        let binding = AnalysisContextBinding {
            context: crate::python_semantic::tests_support::context(),
            project_root_rel: String::new(),
            config_file_rel: None,
        };
        let Err(error) = launcher.launch(&binding) else {
            panic!("a missing runtime must not start something else");
        };
        assert!(
            error.to_string().contains("could not start"),
            "unexpected: {error}"
        );
    }

    #[test]
    fn readiness_waits_until_the_type_server_answers_about_the_project() {
        use crate::python_semantic::jsonrpc::{IgnoreServer, testing};
        use serde_json::json;
        use std::sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        };

        let (to_client, pipe) = testing::Pipe::new();
        let (sink, from_client) = testing::Sink::new();
        let answered = Arc::new(AtomicUsize::new(0));
        let probes = Arc::clone(&answered);
        let ready_at = Arc::new(Mutex::new(None));
        let observed = Arc::clone(&ready_at);

        // A backend that is alive but has not analyzed anything yet:
        // it answers `getSnapshot` and returns `null` search paths,
        // exactly as measured, until its third probe.
        let server = thread::spawn(move || {
            while let Ok(raw) = from_client.recv() {
                let message: serde_json::Value = serde_json::from_slice(&raw).expect("json");
                let Some(id) = message.get("id") else {
                    continue;
                };
                let method = message["method"].as_str().unwrap_or_default();
                let result = match method {
                    "typeServer/getSnapshot" => json!(3),
                    "typeServer/getPythonSearchPaths" => {
                        let seen = probes.fetch_add(1, Ordering::SeqCst);
                        if seen < 2 {
                            serde_json::Value::Null
                        } else {
                            *observed.lock().expect("lock") = Some(seen);
                            json!(["file:///stdlib"])
                        }
                    }
                    _ => serde_json::Value::Null,
                };
                if to_client
                    .send(testing::frame(
                        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    ))
                    .is_err()
                {
                    return;
                }
            }
        });

        let client = Client::new(Box::new(pipe), Box::new(sink), Arc::new(IgnoreServer));
        await_ready(
            &client,
            "file:///w",
            Readiness {
                poll: Duration::from_millis(5),
                timeout: Duration::from_secs(5),
            },
        );
        assert_eq!(
            *ready_at.lock().expect("lock"),
            Some(2),
            "it kept probing while the backend answered null, and stopped as soon as it did not"
        );
        client.close();
        drop(server);
    }

    #[test]
    fn readiness_gives_up_rather_than_blocking_forever() {
        use crate::python_semantic::jsonrpc::{IgnoreServer, testing};

        // A backend that never answers at all. Starting anyway is the
        // honest outcome: the queries will report what they can, and
        // an unready backend is not a failed one.
        let (_to_client, pipe) = testing::Pipe::new();
        let (sink, _from_client) = testing::Sink::new();
        let client = Client::new(Box::new(pipe), Box::new(sink), Arc::new(IgnoreServer));
        let started = Instant::now();
        await_ready(
            &client,
            "file:///w",
            Readiness {
                poll: Duration::from_millis(5),
                timeout: Duration::from_millis(120),
            },
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        client.close();
    }

    #[test]
    fn the_compatibility_class_is_fixed_before_anything_starts() {
        // Task 3 needs the toolchain identity to build an
        // AnalysisContext, and the context key is what selects the
        // runtime -- so the class cannot depend on a running backend.
        assert_eq!(PythonLauncher::compatibility_class(), COMPATIBILITY_CLASS);
    }
}
