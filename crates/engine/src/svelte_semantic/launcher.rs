//! Finding and starting the pinned Svelte language server.
//!
//! The artifact is always a Brainprint-managed or project-local install:
//! a directory containing `node_modules/svelte-language-server`. Nothing
//! here searches `PATH` for a `svelteserver`, and nothing here installs
//! one. A missing install is a typed failure that degrades to Level B,
//! not a fallback.
//!
//! ```text
//! <root>/node_modules/svelte-language-server/package.json   -> the version
//! <root>/node_modules/svelte-language-server/bin/server.js  -> the entry point
//! node <entry> --stdio                                      -> the server
//! ```
//!
//! Unlike the TypeScript 7 backend, which is a native executable, this
//! one is JavaScript and needs a Node runtime. The runtime *command* is
//! supplied by the caller rather than discovered here -- the same
//! arrangement the Python backend uses -- so this module still performs
//! no `PATH` search of its own.

use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
};

use serde_json::Value;

use super::{
    host::{ClientHandler, SvelteHost, SvelteSettings},
    protocol::{
        COMPATIBILITY_CLASS, ProtocolCompatibility, initialize_params, method, path_to_uri,
    },
};
use crate::{
    lsp::jsonrpc::Client,
    runtime::{CancelToken, HostError, SemanticBackendLauncher, SemanticRuntimeHost},
    semantic::{AnalysisContextBinding, SemanticBackendKind},
};

/// The npm package that carries the language server.
pub const PACKAGE: &str = "svelte-language-server";

/// The packages whose versions are part of what a Svelte semantic answer
/// depends on.
///
/// `svelte2tsx` is here because it is what produces the intermediate
/// representation the server type-checks, so its version changes what
/// the server can prove. `svelte` is here because the compiler decides
/// what the component language even is -- runes exist in 5 and not in 4.
/// `typescript` is here because it *is* the language service; see
/// [`TESTED_TYPESCRIPT_VERSION`](super::protocol::TESTED_TYPESCRIPT_VERSION).
pub const COMPANION_PACKAGES: [&str; 3] = ["svelte2tsx", "svelte", "typescript"];

/// Why the pinned install could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// No `node_modules/svelte-language-server` under the configured
    /// root.
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
    /// The package is installed but its entry point is not where this
    /// build expects it.
    EntryPointMissing {
        expected: PathBuf,
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
            Self::EntryPointMissing { expected } => {
                write!(formatter, "no entry point at {}", expected.display())
            }
        }
    }
}

impl Error for InstallError {}

/// A located, project-local Svelte language-tools install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SvelteInstall {
    /// The directory whose `node_modules` was searched.
    pub root: PathBuf,
    /// The JavaScript entry point to run under Node.
    pub entry_point: PathBuf,
    /// The `svelte-language-server` version.
    ///
    /// This is what defines the toolchain identity, and it has to be:
    /// the measured server reports no `serverInfo`, so the manifest is
    /// the only place its version exists.
    pub server_version: String,
    /// `svelte2tsx`, `svelte` and `typescript`, in
    /// [`COMPANION_PACKAGES`] order. A package that is not installed is
    /// recorded as absent rather than as a guessed version.
    pub companions: Vec<(String, Option<String>)>,
}

impl SvelteInstall {
    /// Locate the install under `root`, or say exactly what is missing.
    ///
    /// # Errors
    /// When the package, its manifest or its entry point is absent.
    pub fn locate(root: &Path) -> Result<Self, InstallError> {
        let modules = root.join("node_modules");
        let package = modules.join(PACKAGE);
        let manifest = package.join("package.json");
        if !manifest.is_file() {
            return Err(InstallError::PackageMissing {
                root: root.to_path_buf(),
            });
        }
        let server_version = read_version(&manifest)?.ok_or(InstallError::VersionMissing {
            path: manifest.clone(),
        })?;

        let entry_point = package.join("bin").join("server.js");
        if !entry_point.is_file() {
            return Err(InstallError::EntryPointMissing {
                expected: entry_point,
            });
        }

        let companions = COMPANION_PACKAGES
            .iter()
            .map(|name| {
                let found = read_version(&modules.join(name).join("package.json"))
                    .ok()
                    .flatten();
                ((*name).to_owned(), found)
            })
            .collect();

        Ok(Self {
            root: root.to_path_buf(),
            entry_point,
            server_version,
            companions,
        })
    }

    /// The version of one companion package, if it is installed.
    #[must_use]
    pub fn companion(&self, name: &str) -> Option<&str> {
        self.companions
            .iter()
            .find(|(installed, _)| installed == name)
            .and_then(|(_, version)| version.as_deref())
    }

    /// Whether this adapter may read this install's answers.
    #[must_use]
    pub fn compatibility(&self) -> ProtocolCompatibility {
        ProtocolCompatibility::classify(&self.server_version)
    }
}

fn read_version(manifest: &Path) -> Result<Option<String>, InstallError> {
    let Ok(text) = fs::read_to_string(manifest) else {
        return Ok(None);
    };
    let parsed: Value =
        serde_json::from_str(&text).map_err(|error| InstallError::ManifestUnreadable {
            path: manifest.to_path_buf(),
            detail: error.to_string(),
        })?;
    Ok(parsed
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_owned))
}

/// Starts Svelte backends for the task 2 supervisor.
pub struct SvelteLauncher {
    install: SvelteInstall,
    /// The Node runtime command. Supplied, never searched for.
    node: String,
    settings: SvelteSettings,
}

impl SvelteLauncher {
    #[must_use]
    pub fn new(install: SvelteInstall, node: impl Into<String>) -> Self {
        Self {
            install,
            node: node.into(),
            settings: SvelteSettings,
        }
    }

    #[must_use]
    pub const fn install(&self) -> &SvelteInstall {
        &self.install
    }

    /// The compatibility class, before anything has started.
    #[must_use]
    pub const fn compatibility_class() -> &'static str {
        COMPATIBILITY_CLASS
    }

    /// The exact command line this launcher runs, for the record.
    #[must_use]
    pub fn command_line(&self) -> String {
        format!(
            "{} {} --stdio",
            self.node,
            self.install.entry_point.display()
        )
    }

    /// Start one server for `binding` and complete its handshake.
    ///
    /// # Errors
    /// When the process cannot be started, the install is outside the
    /// compatibility class, or the handshake fails.
    pub fn start(&self, binding: &AnalysisContextBinding) -> Result<SvelteHost, HostError> {
        // The gate, and it is *before* the launch rather than after,
        // because this server publishes no version over the protocol.
        let compatibility = self.install.compatibility();
        if !compatibility.usable() {
            return Err(HostError::new(format!(
                "incompatible semantic backend: {PACKAGE} {} is {compatibility}, \
                 not {COMPATIBILITY_CLASS}",
                self.install.server_version
            )));
        }

        let workspace_root = Path::new(&binding.project_root_rel);
        let mut command = Command::new(&self.node);
        command
            .arg(&self.install.entry_point)
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Kept, not inherited: the server logs its project loading
            // here, and letting that reach Brainprint's own stderr would
            // read as a fault. It is piped *and drained*.
            .stderr(Stdio::piped());
        if workspace_root.is_dir() {
            command.current_dir(workspace_root);
        }

        let mut child: Child = command.spawn().map_err(|error| {
            HostError::new(format!(
                "could not start {} {}: {error}",
                self.node,
                self.install.entry_point.display()
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
            Ok(()) => Ok(SvelteHost::new(
                client,
                Some(child),
                self.install.server_version.clone(),
                handler,
            )),
            Err(error) => {
                client.close();
                let _ = child.kill();
                let _ = child.wait();
                Err(error)
            }
        }
    }

    /// `initialize` / `initialized`.
    ///
    /// Nothing is read out of the result, which is unusual and is the
    /// measurement talking: this server reports no `serverInfo` and no
    /// `positionEncoding`, so the version came from the manifest before
    /// the launch and the encoding is LSP's default.
    fn handshake(&self, client: &Client, workspace_root: &Path) -> Result<(), HostError> {
        let root_uri = path_to_uri(workspace_root);
        let cancel = CancelToken::new();
        client
            .request(
                method::INITIALIZE,
                Some(initialize_params(&root_uri, std::process::id())),
                &cancel,
            )
            .map_err(|failure| HostError::new(format!("initialize failed: {failure}")))?;
        client
            .notify(method::INITIALIZED, Some(serde_json::json!({})))
            .map_err(|failure| HostError::new(format!("initialized failed: {failure}")))?;
        Ok(())
    }
}

/// Read the backend's stderr and throw it away, forever.
///
/// Deadlock avoidance, not tidiness: a piped stream nobody reads fills
/// its kernel buffer and then blocks the writer. This server logs while
/// it loads a project, so an undrained stderr would hang the first
/// request with a healthy connection and a live child.
fn drain_stderr(stderr: std::process::ChildStderr) {
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stderr = stderr;
        let mut scratch = [0_u8; 4096];
        while matches!(stderr.read(&mut scratch), Ok(read) if read > 0) {}
    });
}

impl SemanticBackendLauncher for SvelteLauncher {
    fn kind(&self) -> SemanticBackendKind {
        SemanticBackendKind::Svelte
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

    fn install_tree(root: &Path, version: &str, companions: &[(&str, &str)]) {
        let package = root.join("node_modules").join(PACKAGE);
        fs::create_dir_all(package.join("bin")).expect("package dir");
        fs::write(
            package.join("package.json"),
            format!("{{\"name\":\"{PACKAGE}\",\"version\":\"{version}\"}}"),
        )
        .expect("manifest");
        fs::write(package.join("bin").join("server.js"), b"// server\n").expect("entry");
        for (name, companion_version) in companions {
            let directory = root.join("node_modules").join(name);
            fs::create_dir_all(&directory).expect("companion dir");
            fs::write(
                directory.join("package.json"),
                format!("{{\"name\":\"{name}\",\"version\":\"{companion_version}\"}}"),
            )
            .expect("companion manifest");
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("bp-svelte-launcher-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn a_project_local_install_is_located_with_its_companions() {
        let root = temp_root("located");
        install_tree(
            &root,
            "0.18.4",
            &[
                ("svelte2tsx", "0.7.61"),
                ("svelte", "5.57.1"),
                ("typescript", "6.0.3"),
            ],
        );
        let install = SvelteInstall::locate(&root).expect("located");
        assert_eq!(install.server_version, "0.18.4");
        assert_eq!(install.companion("svelte2tsx"), Some("0.7.61"));
        assert_eq!(install.companion("svelte"), Some("5.57.1"));
        // The Svelte backend's TypeScript is its own, and not the 7.0.2
        // the TS/JS backend runs.
        assert_eq!(
            install.companion("typescript"),
            Some(super::super::protocol::TESTED_TYPESCRIPT_VERSION)
        );
        assert!(install.compatibility().usable());

        let launcher = SvelteLauncher::new(install.clone(), "node");
        let line = launcher.command_line();
        assert!(line.starts_with("node "), "{line}");
        assert!(line.ends_with("bin/server.js --stdio"), "{line}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_package_is_reported_rather_than_searched_for_on_path() {
        let root = temp_root("missing");
        assert_eq!(
            SvelteInstall::locate(&root),
            Err(InstallError::PackageMissing { root: root.clone() })
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_absent_companion_is_absent_rather_than_a_guessed_version() {
        let root = temp_root("no-companions");
        install_tree(&root, "0.18.4", &[]);
        let install = SvelteInstall::locate(&root).expect("located");
        assert_eq!(install.companion("svelte2tsx"), None);
        assert_eq!(install.companion("typescript"), None);
        assert_eq!(install.companions.len(), COMPANION_PACKAGES.len());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_version_outside_the_class_is_refused_before_a_process_is_started() {
        let root = temp_root("incompatible");
        install_tree(&root, "0.19.0", &[]);
        let install = SvelteInstall::locate(&root).expect("located");
        assert_eq!(install.compatibility(), ProtocolCompatibility::Unknown);
        let launcher = SvelteLauncher::new(install, "definitely-not-a-real-node-binary");
        let Err(error) = launcher.start(&AnalysisContextBinding {
            context: crate::svelte_semantic::tests_support::context(),
            project_root_rel: root.to_string_lossy().into_owned(),
            config_file_rel: None,
        }) else {
            panic!("an unreadable protocol must be refused");
        };
        // Refused on the version, not on the missing Node binary: the
        // gate runs first, so an incompatible install never spawns.
        assert!(error.to_string().contains("incompatible"), "{error}");
        let _ = fs::remove_dir_all(&root);
    }
}
