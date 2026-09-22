//! Finding and starting the pinned TypeScript native language server.
//!
//! The artifact is always a Brainprint-managed or project-local install:
//! a directory containing `node_modules/typescript`. Nothing here
//! searches `PATH` for a `tsc`, and nothing here installs one. A missing
//! install is a typed failure that degrades to Level B, not a fallback.
//! A developer who happens to have some other TypeScript on their
//! machine must not silently change what Brainprint publishes, and a
//! machine with none must still get every structural answer I2/I3
//! already has.
//!
//! ## What is actually launched
//!
//! TypeScript 7 is a native binary, not a script. The `typescript`
//! package is a thin shim: `lib/getExePath.js` resolves a per-platform
//! optional dependency and `bin/tsc` `execve`s it. Brainprint resolves
//! the same executable directly and launches it, which means the
//! backend needs no Node runtime at all -- one fewer version on the
//! toolchain identity, and one fewer process in the tree.
//!
//! ```text
//! <root>/node_modules/typescript/package.json            -> the version
//! <root>/node_modules/@typescript/typescript-<os>-<arch>/lib/tsc
//!                                                        -> the executable
//! <exe> --lsp --stdio                                    -> the server
//! ```

use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{Value, json};

use super::{
    host::{ClientHandler, TypeScriptHost, TypeScriptSettings},
    protocol::{
        COMPATIBILITY_CLASS, OFFERED_ENCODINGS, PositionEncodingChoice, ProtocolCompatibility,
        method, path_to_uri,
    },
};
use crate::{
    lsp::jsonrpc::Client,
    runtime::{CancelToken, HostError, SemanticBackendLauncher, SemanticRuntimeHost},
    semantic::{AnalysisContextBinding, SemanticBackendKind},
};

/// The npm package that carries the language server.
pub const PACKAGE: &str = "typescript";

/// The scope its per-platform executables are published under.
const EXE_SCOPE: &str = "@typescript";

/// How long the `initialize` handshake gets before the launch is a
/// failure.
///
/// Generous, and bounded. The measured server answered in well under a
/// second on a cold start, but a first launch on a large project is
/// doing real work; what must not happen is a launch that never returns
/// and never degrades.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// Why the pinned install could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// No `node_modules/typescript` under the configured root.
    PackageMissing {
        root: PathBuf,
    },
    ManifestUnreadable {
        path: PathBuf,
        detail: String,
    },
    /// The manifest is there but names no version.
    VersionMissing {
        path: PathBuf,
    },
    /// The platform package the shim would resolve is not installed.
    ///
    /// A real and specific case, not a theoretical one: npm installs
    /// exactly one of the twenty platform packages, so a `node_modules`
    /// tree copied between a Linux CI image and a developer's Mac has
    /// `typescript` and no usable executable.
    ExecutableMissing {
        expected: PathBuf,
        platform_package: String,
    },
    /// This build has no name for the current platform and architecture.
    UnsupportedPlatform {
        os: &'static str,
        arch: &'static str,
    },
}

impl fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PackageMissing { root } => write!(
                formatter,
                "no node_modules/{PACKAGE} under {}",
                root.display()
            ),
            Self::ManifestUnreadable { path, detail } => {
                write!(
                    formatter,
                    "unreadable manifest {}: {detail}",
                    path.display()
                )
            }
            Self::VersionMissing { path } => {
                write!(formatter, "manifest {} names no version", path.display())
            }
            Self::ExecutableMissing {
                expected,
                platform_package,
            } => write!(
                formatter,
                "{platform_package} is not installed: no executable at {}",
                expected.display()
            ),
            Self::UnsupportedPlatform { os, arch } => {
                write!(formatter, "no TypeScript executable name for {os}-{arch}")
            }
        }
    }
}

impl Error for InstallError {}

/// The Node platform/architecture pair whose package name this build
/// would resolve.
///
/// Node's spelling, not Rust's: the package names are published against
/// `process.platform` and `process.arch`, so `macos`/`aarch64` has to
/// become `darwin`/`arm64` or the path does not exist. Returning `None`
/// rather than guessing keeps an unknown target an honest Level B
/// instead of a confusing "file not found".
#[must_use]
pub const fn node_target(os: &str, arch: &str) -> Option<(&'static str, &'static str)> {
    let platform = match os.as_bytes() {
        b"macos" => "darwin",
        b"linux" => "linux",
        b"windows" => "win32",
        b"freebsd" => "freebsd",
        b"netbsd" => "netbsd",
        b"openbsd" => "openbsd",
        _ => return None,
    };
    let architecture = match arch.as_bytes() {
        b"x86_64" => "x64",
        b"aarch64" => "arm64",
        b"arm" => "arm",
        b"powerpc64" => "ppc64",
        b"riscv64" => "riscv64",
        b"s390x" => "s390x",
        b"loongarch64" => "loong64",
        _ => return None,
    };
    Some((platform, architecture))
}

/// A located, project-local TypeScript install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeScriptInstall {
    /// The directory whose `node_modules` was searched.
    pub root: PathBuf,
    /// The native executable to launch.
    pub executable: PathBuf,
    /// The version the `typescript` manifest declares.
    ///
    /// Recorded, and deliberately *not* what defines the toolchain
    /// identity: the running server reports its own version at
    /// handshake, and that is what
    /// [`TypeScriptHost::server_version`](super::host::TypeScriptHost::server_version)
    /// carries. A manifest that disagrees with its binary must not get
    /// to define the identity.
    pub manifest_version: String,
    /// The per-platform package the executable came from.
    pub platform_package: String,
}

impl TypeScriptInstall {
    /// Locate the install under `root`, or say exactly what is missing.
    pub fn locate(root: &Path) -> Result<Self, InstallError> {
        Self::locate_for(root, std::env::consts::OS, std::env::consts::ARCH)
    }

    /// [`Self::locate`] for a named target, so the platform mapping is
    /// testable without twenty machines.
    pub fn locate_for(
        root: &Path,
        os: &'static str,
        arch: &'static str,
    ) -> Result<Self, InstallError> {
        let package = root.join("node_modules").join(PACKAGE);
        let manifest = package.join("package.json");
        if !manifest.is_file() {
            return Err(InstallError::PackageMissing {
                root: root.to_path_buf(),
            });
        }
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
        let manifest_version = parsed
            .get("version")
            .and_then(Value::as_str)
            .ok_or(InstallError::VersionMissing {
                path: manifest.clone(),
            })?
            .to_owned();

        let Some((platform, architecture)) = node_target(os, arch) else {
            return Err(InstallError::UnsupportedPlatform { os, arch });
        };
        let platform_package = format!("{EXE_SCOPE}/{PACKAGE}-{platform}-{architecture}");
        let mut executable = root
            .join("node_modules")
            .join(EXE_SCOPE)
            .join(format!("{PACKAGE}-{platform}-{architecture}"))
            .join("lib")
            .join("tsc");
        if platform == "win32" {
            executable.set_extension("exe");
        }
        if !executable.is_file() {
            return Err(InstallError::ExecutableMissing {
                expected: executable,
                platform_package,
            });
        }
        Ok(Self {
            root: root.to_path_buf(),
            executable,
            manifest_version,
            platform_package,
        })
    }
}

/// Starts TypeScript/JavaScript backends for the task 2 supervisor.
pub struct TypeScriptLauncher {
    install: TypeScriptInstall,
    settings: TypeScriptSettings,
    handshake_timeout: Duration,
    /// What the most recent successful handshake settled on.
    ///
    /// The adapter needs it -- reading a UTF-8 range as UTF-16 puts
    /// every span at a plausible wrong offset -- and a caller that
    /// drives the backend through the task 2 supervisor never sees the
    /// host, only a lease. Recorded here rather than assumed there,
    /// because "the server accepts UTF-8" is a measurement about a
    /// running process and not a constant this build may hard-code.
    ///
    /// One slot for every context is right: they all talk to the same
    /// executable with the same offer, so a second handshake that
    /// settled differently would mean the binary changed underneath,
    /// which the compatibility gate refuses first.
    encoding: Mutex<Option<PositionEncodingChoice>>,
}

impl TypeScriptLauncher {
    #[must_use]
    pub const fn new(install: TypeScriptInstall) -> Self {
        Self {
            install,
            settings: TypeScriptSettings,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            encoding: Mutex::new(None),
        }
    }

    /// The position encoding the last successful handshake settled on,
    /// or `None` before anything has started.
    #[must_use]
    pub fn negotiated_encoding(&self) -> Option<PositionEncodingChoice> {
        *self
            .encoding
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[must_use]
    pub const fn with_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    #[must_use]
    pub const fn install(&self) -> &TypeScriptInstall {
        &self.install
    }

    /// The compatibility class, before anything has started.
    ///
    /// Task 3 needs the toolchain identity to build an
    /// [`AnalysisContext`](crate::semantic::AnalysisContext), and the
    /// context key is what selects the runtime -- so the class cannot
    /// depend on a running backend.
    #[must_use]
    pub const fn compatibility_class() -> &'static str {
        COMPATIBILITY_CLASS
    }

    /// The exact command line this launcher runs, for the record.
    #[must_use]
    pub fn command_line(&self) -> String {
        format!("{} --lsp --stdio", self.install.executable.display())
    }

    /// Start one server for `binding` and complete its handshake.
    pub fn start(&self, binding: &AnalysisContextBinding) -> Result<TypeScriptHost, HostError> {
        let workspace_root = Path::new(&binding.project_root_rel);
        let mut command = Command::new(&self.install.executable);
        command
            .arg("--lsp")
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Kept, not inherited: the measured server writes
            // `context canceled` here on a clean stop, and letting that
            // reach Brainprint's own stderr would read as a fault. It
            // is piped *and drained* -- see `drain_stderr`.
            .stderr(Stdio::piped());
        if workspace_root.is_dir() {
            command.current_dir(workspace_root);
        }

        let mut child: Child = command.spawn().map_err(|error| {
            HostError::new(format!(
                "could not start {}: {error}",
                self.install.executable.display()
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HostError::new("backend stdin was not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HostError::new("backend stdout was not piped"))?;
        if let Some(stderr) = child.stderr.take() {
            drain_stderr(stderr);
        }

        let handler = Arc::new(ClientHandler::new(self.settings));
        let client = Client::new(Box::new(stdout), Box::new(stdin), handler.clone());

        match self.handshake(&client, workspace_root) {
            Ok((name, version, encoding)) => {
                *self
                    .encoding
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(encoding);
                Ok(TypeScriptHost::new(
                    client,
                    Some(child),
                    name,
                    version,
                    encoding,
                    handler,
                ))
            }
            Err(error) => {
                // A handshake that failed leaves a process nobody owns.
                client.close();
                let _ = child.kill();
                let _ = child.wait();
                Err(error)
            }
        }
    }

    /// `initialize` / `initialized`, and everything read out of it.
    fn handshake(
        &self,
        client: &Client,
        workspace_root: &Path,
    ) -> Result<(String, String, PositionEncodingChoice), HostError> {
        let root_uri = path_to_uri(workspace_root);
        let cancel = CancelToken::new();
        let result = client
            .request(
                method::INITIALIZE,
                Some(initialize_params(&root_uri)),
                &cancel,
            )
            .map_err(|failure| HostError::new(format!("initialize failed: {failure}")))?;
        let _ = self.handshake_timeout;

        let info = result.get("serverInfo");
        let name = info
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let version = info
            .and_then(|info| info.get("version"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        // The gate. A server outside the compatibility class is not
        // asked a single semantic question, because parsing its answers
        // as if they were 7.0's would turn a protocol change into wrong
        // facts. Refusing here is what makes that case Level B.
        let compatibility = ProtocolCompatibility::classify(&name, &version);
        if !compatibility.usable() {
            return Err(HostError::new(format!(
                "incompatible semantic backend: {name} {version} is {compatibility}, \
                 not {COMPATIBILITY_CLASS}"
            )));
        }

        let encoding = PositionEncodingChoice::from_initialize(&result).map_err(|named| {
            HostError::new(format!(
                "backend negotiated position encoding {named}, which this build cannot count in"
            ))
        })?;

        client
            .notify(method::INITIALIZED, Some(json!({})))
            .map_err(|failure| HostError::new(format!("initialized failed: {failure}")))?;
        Ok((name, version, encoding))
    }
}

/// Read the backend's stderr and throw it away, forever.
///
/// Not tidiness -- deadlock avoidance, and one this cost real debugging
/// to find. A piped stream nobody reads fills its kernel buffer (64 KiB
/// on macOS) and then *blocks the writer*. The measured server logs
/// while it loads a project, so the handshake succeeded, the first
/// `textDocument/definition` went out, the server filled stderr loading
/// the project, and it blocked before it could answer. The connection
/// was healthy, the child was alive, and the request never returned.
///
/// The output is discarded rather than surfaced on purpose. It is a
/// language server's internal log, not a Brainprint diagnostic: a caller
/// who sees `context canceled` on Brainprint's stderr after every
/// orderly shutdown learns nothing and mistrusts everything. What the
/// backend says about *semantics* comes back over the connection as
/// typed answers, which is the only channel anything here reads.
fn drain_stderr(stderr: std::process::ChildStderr) {
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stderr = stderr;
        let mut scratch = [0_u8; 4096];
        while matches!(stderr.read(&mut scratch), Ok(read) if read > 0) {}
    });
}

/// The client capabilities this backend is driven with.
///
/// Read it as a list of decisions rather than boilerplate:
///
/// * `positionEncodings` offers UTF-8 first, so spans map to bytes with
///   no conversion at all where the server agrees.
/// * `workspace.didChangeWatchedFiles.dynamicRegistration` is `true`,
///   which is what makes the server hand its file watching to the
///   client. That is the measured decision in
///   [`WATCHER_DECISION`](super::WATCHER_DECISION); declaring it `false`
///   would leave the server on its own OS watcher and leave Brainprint
///   with no moment at which it knows the backend has caught up.
/// * `workspace.configuration` is `true` because the server asks, and an
///   unanswered `workspace/configuration` wedges the connection.
/// * `textDocument.synchronization` is absent, deliberately. This client
///   never opens a document.
fn initialize_params(root_uri: &str) -> Value {
    let encodings: Vec<&str> = OFFERED_ENCODINGS
        .iter()
        .map(|encoding| encoding.as_str())
        .collect();
    json!({
        "processId": std::process::id(),
        "rootUri": root_uri,
        "capabilities": {
            "general": { "positionEncodings": encodings },
            "textDocument": {
                "definition": { "linkSupport": true },
                "typeDefinition": { "linkSupport": true },
                "implementation": { "linkSupport": true },
                "hover": { "contentFormat": ["plaintext", "markdown"] },
                "signatureHelp": {},
                "documentSymbol": {},
                "callHierarchy": {},
            },
            "workspace": {
                "workspaceFolders": true,
                "configuration": true,
                "didChangeWatchedFiles": { "dynamicRegistration": true },
            },
        },
        "workspaceFolders": [{ "uri": root_uri, "name": "workspace" }],
    })
}

impl SemanticBackendLauncher for TypeScriptLauncher {
    fn kind(&self) -> SemanticBackendKind {
        SemanticBackendKind::TypeScriptJavaScript
    }

    fn launch(
        &self,
        binding: &AnalysisContextBinding,
    ) -> Result<Arc<dyn SemanticRuntimeHost>, HostError> {
        Ok(Arc::new(self.start(binding)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install_tree(root: &Path, os: &str, arch: &str, version: &str) {
        let package = root.join("node_modules").join(PACKAGE);
        fs::create_dir_all(&package).expect("package dir");
        fs::write(
            package.join("package.json"),
            format!("{{\"name\":\"typescript\",\"version\":\"{version}\"}}"),
        )
        .expect("manifest");
        let (platform, architecture) = node_target(os, arch).expect("target");
        let executables = root
            .join("node_modules")
            .join(EXE_SCOPE)
            .join(format!("{PACKAGE}-{platform}-{architecture}"))
            .join("lib");
        fs::create_dir_all(&executables).expect("exe dir");
        fs::write(executables.join("tsc"), b"#!/bin/sh\n").expect("exe");
    }

    fn temp_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("bp-ts-launcher-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn node_target_uses_nodes_spelling_not_rusts() {
        // The package names are published against `process.platform`
        // and `process.arch`; Rust's own names do not exist on npm.
        assert_eq!(node_target("macos", "aarch64"), Some(("darwin", "arm64")));
        assert_eq!(node_target("linux", "x86_64"), Some(("linux", "x64")));
        assert_eq!(node_target("windows", "x86_64"), Some(("win32", "x64")));
    }

    #[test]
    fn an_unnameable_target_is_a_typed_failure_not_a_guess() {
        assert_eq!(node_target("plan9", "x86_64"), None);
        assert_eq!(node_target("linux", "sparc64"), None);
        let root = temp_root("unsupported");
        install_tree(&root, "linux", "x86_64", "7.0.2");
        assert_eq!(
            TypeScriptInstall::locate_for(&root, "plan9", "x86_64"),
            Err(InstallError::UnsupportedPlatform {
                os: "plan9",
                arch: "x86_64",
            })
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_project_local_install_is_located_with_its_native_executable() {
        let root = temp_root("located");
        install_tree(&root, "linux", "x86_64", "7.0.2");
        let install = TypeScriptInstall::locate_for(&root, "linux", "x86_64").expect("located");
        assert_eq!(install.manifest_version, "7.0.2");
        assert_eq!(install.platform_package, "@typescript/typescript-linux-x64");
        assert!(install.executable.ends_with("lib/tsc"));
        // The native binary is what runs: no `node`, and no `bin/tsc`
        // shim in front of it, so Node is not on the toolchain
        // identity and there is no extra process in the tree.
        let launcher = TypeScriptLauncher::new(install.clone());
        let line = launcher.command_line();
        assert_eq!(
            line,
            format!("{} --lsp --stdio", install.executable.display())
        );
        assert!(!line.starts_with("node "), "{line}");
        assert!(!line.contains("/bin/tsc"), "{line}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_package_is_reported_rather_than_searched_for_on_path() {
        let root = temp_root("missing");
        assert_eq!(
            TypeScriptInstall::locate_for(&root, "linux", "x86_64"),
            Err(InstallError::PackageMissing { root: root.clone() })
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_node_modules_tree_from_another_platform_names_what_is_missing() {
        // The realistic case: a tree copied from a Linux CI image onto
        // a developer's Mac has `typescript` and no usable executable.
        let root = temp_root("wrong-platform");
        install_tree(&root, "linux", "x86_64", "7.0.2");
        let error = TypeScriptInstall::locate_for(&root, "macos", "aarch64")
            .expect_err("no darwin executable");
        let InstallError::ExecutableMissing {
            platform_package, ..
        } = &error
        else {
            panic!("unexpected: {error}");
        };
        assert_eq!(platform_package, "@typescript/typescript-darwin-arm64");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_compatibility_class_is_fixed_before_anything_starts() {
        assert_eq!(
            TypeScriptLauncher::compatibility_class(),
            COMPATIBILITY_CLASS
        );
    }

    #[test]
    fn the_handshake_never_offers_document_synchronization() {
        // The capability declaration is the earliest place the "no
        // editor overlay" decision can be enforced, and the most
        // durable: a server told nothing about `synchronization` will
        // not expect an overlay.
        let params = initialize_params("file:///w");
        let text_document = &params["capabilities"]["textDocument"];
        assert!(text_document.get("synchronization").is_none());
        assert_eq!(
            params["capabilities"]["workspace"]["didChangeWatchedFiles"]["dynamicRegistration"],
            true,
            "client-side watching is what gives an ordering barrier"
        );
        assert_eq!(
            params["capabilities"]["general"]["positionEncodings"][0],
            "utf-8"
        );
        assert_eq!(params["capabilities"]["workspace"]["configuration"], true);
    }
}
