//! Finding and starting the pinned Roslyn language server.
//!
//! The artifact is always a Brainprint-managed or project-local install:
//! a restored `Microsoft.CodeAnalysis.LanguageServer.<rid>` package.
//! Nothing here searches `PATH`, and nothing here installs one -- the
//! spike's `restore.sh` does that, outside production code. A missing
//! install is a typed failure that degrades to Level B.
//!
//! ```text
//! <root>/packages/microsoft.codeanalysis.languageserver.<rid>/<version>/
//!     content/LanguageServer/<rid>/Microsoft.CodeAnalysis.LanguageServer
//! ```
//!
//! The executable is a self-contained apphost, so unlike the Python and
//! Svelte backends there is no separate runtime command to supply. It
//! still needs a .NET runtime installed; a missing one surfaces as a
//! failed launch, which is Level B, not a crash.

use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
};

use super::{
    host::{CSharpHost, CSharpSettings, ClientHandler},
    protocol::{
        COMPATIBILITY_CLASS, ProjectExecutionTrust, ProtocolCompatibility, initialize_params,
        method, path_to_uri,
    },
};
use crate::{
    lsp::jsonrpc::Client,
    runtime::{CancelToken, HostError, SemanticBackendLauncher, SemanticRuntimeHost},
    semantic::{AnalysisContextBinding, SemanticBackendKind},
};

/// The package family that carries the language server.
pub const PACKAGE_PREFIX: &str = "microsoft.codeanalysis.languageserver";

/// Why the pinned install could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// No restored language-server package under the configured root.
    PackageMissing { root: PathBuf },
    /// The package is there but its executable is not where this build
    /// expects it -- a tree restored for another runtime identifier, for
    /// instance.
    ExecutableMissing { expected: PathBuf },
    /// This build has no runtime identifier for the current platform.
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
                "no restored {PACKAGE_PREFIX} package under {}",
                root.display()
            ),
            Self::ExecutableMissing { expected } => {
                write!(formatter, "no language server at {}", expected.display())
            }
            Self::UnsupportedPlatform { os, arch } => {
                write!(formatter, "no Roslyn runtime identifier for {os}-{arch}")
            }
        }
    }
}

impl Error for InstallError {}

/// The .NET runtime identifier for a platform.
///
/// .NET's spelling, not Rust's: the packages are published per RID, so
/// `macos`/`aarch64` has to become `osx-arm64` or the path does not
/// exist. `None` rather than a guess keeps an unknown target an honest
/// Level B instead of a confusing "file not found".
#[must_use]
pub const fn runtime_identifier(os: &str, arch: &str) -> Option<&'static str> {
    match (os.as_bytes(), arch.as_bytes()) {
        (b"macos", b"aarch64") => Some("osx-arm64"),
        (b"macos", b"x86_64") => Some("osx-x64"),
        (b"linux", b"aarch64") => Some("linux-arm64"),
        (b"linux", b"x86_64") => Some("linux-x64"),
        (b"windows", b"x86_64") => Some("win-x64"),
        (b"windows", b"aarch64") => Some("win-arm64"),
        _ => None,
    }
}

/// A located, project-local Roslyn language-server install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSharpInstall {
    pub root: PathBuf,
    pub executable: PathBuf,
    /// The package version. This is what defines the toolchain
    /// identity -- the server reports none over the protocol.
    pub server_version: String,
    pub runtime_identifier: String,
}

impl CSharpInstall {
    /// Locate the install under `root`, or say exactly what is missing.
    ///
    /// # Errors
    /// When the package or its executable is absent, or the platform has
    /// no runtime identifier.
    pub fn locate(root: &Path) -> Result<Self, InstallError> {
        Self::locate_for(root, std::env::consts::OS, std::env::consts::ARCH)
    }

    /// [`Self::locate`] for a named target, so the platform mapping is
    /// testable without six machines.
    ///
    /// # Errors
    /// As [`Self::locate`].
    pub fn locate_for(
        root: &Path,
        os: &'static str,
        arch: &'static str,
    ) -> Result<Self, InstallError> {
        let Some(rid) = runtime_identifier(os, arch) else {
            return Err(InstallError::UnsupportedPlatform { os, arch });
        };
        let package = root
            .join("packages")
            .join(format!("{PACKAGE_PREFIX}.{rid}"));
        // One restored version per package directory; take the highest
        // by name so a tree with two is deterministic rather than
        // whichever the filesystem lists first.
        let mut versions: Vec<PathBuf> = fs::read_dir(&package)
            .map_err(|_| InstallError::PackageMissing {
                root: root.to_path_buf(),
            })?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_dir())
            .collect();
        versions.sort();
        let Some(version_dir) = versions.pop() else {
            return Err(InstallError::PackageMissing {
                root: root.to_path_buf(),
            });
        };
        let server_version = version_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();

        let mut executable = version_dir
            .join("content")
            .join("LanguageServer")
            .join(rid)
            .join("Microsoft.CodeAnalysis.LanguageServer");
        if rid.starts_with("win") {
            executable.set_extension("exe");
        }
        if !executable.is_file() {
            return Err(InstallError::ExecutableMissing {
                expected: executable,
            });
        }
        Ok(Self {
            root: root.to_path_buf(),
            executable,
            server_version,
            runtime_identifier: rid.to_owned(),
        })
    }

    /// Whether this adapter may read this install's answers.
    #[must_use]
    pub fn compatibility(&self) -> ProtocolCompatibility {
        ProtocolCompatibility::classify(&self.server_version)
    }
}

/// Starts C# backends for the task 2 supervisor.
pub struct CSharpLauncher {
    install: CSharpInstall,
    /// What this Workspace has been decided to permit. Carried on the
    /// launcher so every host it starts inherits the same answer.
    trust: ProjectExecutionTrust,
    settings: CSharpSettings,
    log_directory: PathBuf,
}

impl CSharpLauncher {
    /// A launcher for a Workspace whose trust has been decided.
    ///
    /// There is no constructor that defaults to trusted. The decision is
    /// an input, and a caller that has not made one passes
    /// [`ProjectExecutionTrust::Untrusted`].
    #[must_use]
    pub fn new(
        install: CSharpInstall,
        trust: ProjectExecutionTrust,
        log_directory: impl Into<PathBuf>,
    ) -> Self {
        Self {
            install,
            trust,
            settings: CSharpSettings,
            log_directory: log_directory.into(),
        }
    }

    #[must_use]
    pub const fn install(&self) -> &CSharpInstall {
        &self.install
    }

    #[must_use]
    pub const fn trust(&self) -> ProjectExecutionTrust {
        self.trust
    }

    #[must_use]
    pub const fn compatibility_class() -> &'static str {
        COMPATIBILITY_CLASS
    }

    /// The exact command line this launcher runs, for the record.
    ///
    /// `--logLevel` and `--extensionLogDirectory` are not optional: the
    /// server refuses to start without them.
    #[must_use]
    pub fn command_line(&self) -> String {
        format!(
            "{} --stdio --logLevel Warning --extensionLogDirectory {}",
            self.install.executable.display(),
            self.log_directory.display()
        )
    }

    /// Start one server for `binding` and complete its handshake.
    ///
    /// # Errors
    /// When the install is outside the compatibility class, the process
    /// cannot be started, or the handshake fails.
    pub fn start(&self, binding: &AnalysisContextBinding) -> Result<CSharpHost, HostError> {
        let compatibility = self.install.compatibility();
        if !compatibility.usable() {
            return Err(HostError::new(format!(
                "incompatible semantic backend: {} is {compatibility}, not {COMPATIBILITY_CLASS}",
                self.install.server_version
            )));
        }
        let _ = fs::create_dir_all(&self.log_directory);

        let workspace_root = Path::new(&binding.project_root_rel);
        let mut command = Command::new(&self.install.executable);
        command
            .arg("--stdio")
            .arg("--logLevel")
            .arg("Warning")
            .arg("--extensionLogDirectory")
            .arg(&self.log_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
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
            Ok(()) => Ok(CSharpHost::new(
                client,
                Some(child),
                self.install.server_version.clone(),
                self.trust,
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
    /// Note what is *not* here: no project is opened. Loading is a
    /// separate, trust-gated step, so a handshake never executes project
    /// code -- which is what makes an untrusted Workspace usable at all.
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
/// Deadlock avoidance: a piped stream nobody reads fills its kernel
/// buffer and blocks the writer. Roslyn logs while it loads a project,
/// which is exactly when a blocked writer would hang the first query.
fn drain_stderr(stderr: std::process::ChildStderr) {
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stderr = stderr;
        let mut scratch = [0_u8; 4096];
        while matches!(stderr.read(&mut scratch), Ok(read) if read > 0) {}
    });
}

impl SemanticBackendLauncher for CSharpLauncher {
    fn kind(&self) -> SemanticBackendKind {
        SemanticBackendKind::CSharp
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

    fn install_tree(root: &Path, rid: &str, version: &str) {
        let server = root
            .join("packages")
            .join(format!("{PACKAGE_PREFIX}.{rid}"))
            .join(version)
            .join("content")
            .join("LanguageServer")
            .join(rid);
        fs::create_dir_all(&server).expect("server dir");
        fs::write(
            server.join("Microsoft.CodeAnalysis.LanguageServer"),
            b"#!/bin/sh\n",
        )
        .expect("executable");
    }

    fn temp_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("bp-cs-launcher-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn runtime_identifiers_use_dotnets_spelling_not_rusts() {
        assert_eq!(runtime_identifier("macos", "aarch64"), Some("osx-arm64"));
        assert_eq!(runtime_identifier("linux", "x86_64"), Some("linux-x64"));
        assert_eq!(runtime_identifier("windows", "aarch64"), Some("win-arm64"));
        assert_eq!(runtime_identifier("plan9", "x86_64"), None);
    }

    #[test]
    fn a_restored_install_is_located_with_its_version() {
        let root = temp_root("located");
        install_tree(&root, "osx-arm64", "5.4.0-2.26179.14");
        let install = CSharpInstall::locate_for(&root, "macos", "aarch64").expect("located");
        assert_eq!(install.server_version, "5.4.0-2.26179.14");
        assert!(install.compatibility().usable());

        let launcher =
            CSharpLauncher::new(install, ProjectExecutionTrust::Untrusted, root.join("logs"));
        let line = launcher.command_line();
        // Both are required arguments; the server refuses to start
        // without them.
        assert!(line.contains("--logLevel"), "{line}");
        assert!(line.contains("--extensionLogDirectory"), "{line}");
        assert!(line.contains("--stdio"), "{line}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn two_restored_versions_resolve_deterministically() {
        let root = temp_root("two-versions");
        install_tree(&root, "osx-arm64", "5.4.0-2.26100.1");
        install_tree(&root, "osx-arm64", "5.4.0-2.26179.14");
        let install = CSharpInstall::locate_for(&root, "macos", "aarch64").expect("located");
        assert_eq!(install.server_version, "5.4.0-2.26179.14");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_package_is_reported_rather_than_searched_for_on_path() {
        let root = temp_root("missing");
        assert_eq!(
            CSharpInstall::locate_for(&root, "macos", "aarch64"),
            Err(InstallError::PackageMissing { root: root.clone() })
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_tree_restored_for_another_platform_names_what_is_missing() {
        let root = temp_root("wrong-rid");
        install_tree(&root, "linux-x64", "5.4.0-2.26179.14");
        let error =
            CSharpInstall::locate_for(&root, "macos", "aarch64").expect_err("no osx executable");
        assert!(
            matches!(error, InstallError::PackageMissing { .. }),
            "{error}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_launcher_carries_the_trust_decision_rather_than_defaulting_to_yes() {
        let root = temp_root("trust");
        install_tree(&root, "osx-arm64", "5.4.0-2.26179.14");
        let install = CSharpInstall::locate_for(&root, "macos", "aarch64").expect("located");
        let untrusted = CSharpLauncher::new(
            install.clone(),
            ProjectExecutionTrust::Untrusted,
            root.join("logs"),
        );
        assert_eq!(untrusted.trust(), ProjectExecutionTrust::Untrusted);
        let trusted =
            CSharpLauncher::new(install, ProjectExecutionTrust::Trusted, root.join("logs"));
        assert_eq!(trusted.trust(), ProjectExecutionTrust::Trusted);
        let _ = fs::remove_dir_all(&root);
    }
}
