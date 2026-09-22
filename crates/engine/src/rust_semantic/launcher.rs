//! Starting rust-analyzer from an executable someone named.
//!
//! Rust differs from Node and .NET in where the backend lives: it is
//! usually owned by the *toolchain* rather than restored into the
//! project. That changes where the path comes from and nothing else
//! about the rule — production Brainprint is handed an explicit
//! executable and uses that one.
//!
//! What this module will not do, and each of these is a way the rule
//! could be lost:
//!
//! ```text
//! search PATH                       a different rust-analyzer per machine
//! run `rustup which rust-analyzer`  a toolchain command, at analysis time
//! rustup component add …            installing, to make a test pass
//! rustup toolchain install …        installing a compiler
//! download rust-src                 installing a component
//! ```
//!
//! Discovering the path is a *setup* step, and it lives in
//! `scripts/rust_semantic_spike/install.sh` where a person runs it.

use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    thread,
};

use super::{
    host::{ClientHandler, RustHost, RustSettings},
    protocol::{
        COMPATIBILITY_CLASS, ProjectExecutionTrust, ProtocolCompatibility, initialize_params,
        method, path_to_uri,
    },
};
use crate::{
    lsp::jsonrpc::Client,
    runtime::{CancelToken, HostError, SemanticBackendLauncher, SemanticRuntimeHost},
    semantic::AnalysisContextBinding,
};

/// Why an install could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// The named executable is not there, or is not a file.
    ExecutableMissing { named: PathBuf },
    /// It is there and would not say what it is.
    Unreadable { named: PathBuf, detail: String },
}

impl fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExecutableMissing { named } => write!(
                formatter,
                "no rust-analyzer at {}; Brainprint uses the executable it is given and \
                 never searches PATH or installs one -- see \
                 scripts/rust_semantic_spike/install.sh",
                named.display()
            ),
            Self::Unreadable { named, detail } => write!(
                formatter,
                "{} did not report a version: {detail}",
                named.display()
            ),
        }
    }
}

impl Error for InstallError {}

/// The toolchain a Rust semantic answer depends on.
///
/// Both the backend and the compiler, because rust-analyzer shipped
/// inside a toolchain reports the *toolchain's* version, and one
/// installed separately does not. Recording both means the fingerprint
/// is right either way rather than right by coincidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustInstall {
    /// The executable, exactly as it was given.
    pub executable: PathBuf,
    /// What `rust-analyzer --version` said.
    pub server_version: String,
    /// What `rustc --version` said, when a compiler was named.
    pub rustc_version: String,
    /// The host triple, which decides what `cfg(target_os)` means.
    pub host_triple: String,
    /// The sysroot, as an identity rather than a path to index.
    pub sysroot_identity: String,
    /// Whether the standard library's source is present.
    ///
    /// Its absence narrows navigation *into* std and nothing else, so
    /// it is recorded rather than treated as a failure.
    pub rust_src: bool,
}

impl RustInstall {
    /// Use the executable at `executable`, asking it what it is.
    ///
    /// # Errors
    /// When it is absent, or will not report a version.
    pub fn at(executable: impl Into<PathBuf>) -> Result<Self, InstallError> {
        let executable = executable.into();
        if !executable.is_file() {
            return Err(InstallError::ExecutableMissing { named: executable });
        }
        let server_version = ask(&executable, &["--version"])?;
        Ok(Self {
            executable,
            server_version: trim_version(&server_version),
            rustc_version: String::new(),
            host_triple: String::new(),
            sysroot_identity: String::new(),
            rust_src: false,
        })
    }

    /// Record the compiler this Workspace's analysis runs against.
    ///
    /// The values are supplied rather than discovered: asking `rustc`
    /// here would be the same toolchain command this module refuses to
    /// run at analysis time.
    #[must_use]
    pub fn with_toolchain(
        mut self,
        rustc_version: impl Into<String>,
        host_triple: impl Into<String>,
        sysroot: &Path,
    ) -> Self {
        self.rustc_version = rustc_version.into();
        self.host_triple = host_triple.into();
        // The sysroot's *name*, not its absolute path: two machines with
        // the same toolchain must fingerprint the same, and a home
        // directory is a locator.
        self.sysroot_identity = sysroot
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.rust_src = sysroot.join("lib/rustlib/src/rust/library").is_dir();
        self
    }

    /// Whether this adapter may read this server's answers.
    #[must_use]
    pub fn compatibility(&self) -> ProtocolCompatibility {
        ProtocolCompatibility::classify(&self.server_version)
    }
}

fn ask(executable: &Path, args: &[&str]) -> Result<String, InstallError> {
    let output = Command::new(executable)
        .args(args)
        .output()
        .map_err(|error| InstallError::Unreadable {
            named: executable.to_path_buf(),
            detail: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(InstallError::Unreadable {
            named: executable.to_path_buf(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// `rust-analyzer 1.98.1 (48a229ce 2026-09-01)` -> `1.98.1`.
fn trim_version(reported: &str) -> String {
    reported
        .split_whitespace()
        .find(|word| {
            word.chars()
                .next()
                .is_some_and(|first| first.is_ascii_digit())
        })
        .unwrap_or(reported)
        .to_owned()
}

// ---------------------------------------------------------------------
// Launcher
// ---------------------------------------------------------------------

/// Starts Rust backends for the task 2 supervisor.
pub struct RustLauncher {
    install: RustInstall,
    /// What this Workspace has been decided to permit. Carried on the
    /// launcher so every host it starts inherits the same answer.
    trust: ProjectExecutionTrust,
    settings: RustSettings,
}

impl RustLauncher {
    /// A launcher for a Workspace whose trust has been decided.
    ///
    /// There is no constructor that defaults to trusted. The decision
    /// is an input, and a caller that has not made one passes
    /// [`ProjectExecutionTrust::Untrusted`].
    #[must_use]
    pub const fn new(install: RustInstall, trust: ProjectExecutionTrust) -> Self {
        Self {
            install,
            trust,
            settings: RustSettings,
        }
    }

    #[must_use]
    pub const fn install(&self) -> &RustInstall {
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
    /// One word. rust-analyzer speaks LSP on stdio with no arguments,
    /// and everything else it needs arrives in `initialize`.
    #[must_use]
    pub fn command_line(&self) -> String {
        self.install.executable.display().to_string()
    }

    /// Start one server for `binding` and complete its handshake.
    ///
    /// # Errors
    /// When the install is outside the compatibility class, the process
    /// cannot be started, or the handshake fails.
    pub fn start(&self, binding: &AnalysisContextBinding) -> Result<RustHost, HostError> {
        let compatibility = self.install.compatibility();
        if !compatibility.usable() {
            return Err(HostError::new(format!(
                "incompatible semantic backend: {} is {compatibility}, not {COMPATIBILITY_CLASS}",
                self.install.server_version
            )));
        }

        let workspace_root = Path::new(&binding.project_root_rel);
        let mut command = Command::new(&self.install.executable);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if workspace_root.is_dir() {
            command.current_dir(workspace_root);
        }
        // Cargo must not reach the network to answer a question about
        // source that is already here. This is the switch that means
        // it, rather than `cargo.noDeps`, which only empties the crate
        // graph.
        command.env("CARGO_NET_OFFLINE", "true");

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
            Ok(()) => Ok(RustHost::new(
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
    /// The handshake carries the P0 configuration, which is what makes
    /// the load safe rather than a later message: the server reads
    /// manifests as soon as it is initialized, so "disable build
    /// scripts afterwards" would be too late.
    fn handshake(&self, client: &Client, workspace_root: &Path) -> Result<(), HostError> {
        let root_uri = path_to_uri(workspace_root);
        let name = workspace_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "workspace".to_owned());
        let cancel = CancelToken::new();
        client
            .request(
                method::INITIALIZE,
                Some(initialize_params(&root_uri, std::process::id(), &name)),
                &cancel,
            )
            .map_err(|failure| HostError::new(failure.to_string()))?;
        client
            .notify(method::INITIALIZED, Some(serde_json::json!({})))
            .map_err(|failure| HostError::new(failure.to_string()))?;
        Ok(())
    }
}

impl SemanticBackendLauncher for RustLauncher {
    fn kind(&self) -> crate::semantic::SemanticBackendKind {
        crate::semantic::SemanticBackendKind::Rust
    }

    fn launch(
        &self,
        binding: &AnalysisContextBinding,
    ) -> Result<Arc<dyn SemanticRuntimeHost>, HostError> {
        Ok(Arc::new(self.start(binding)?))
    }
}

/// Read the backend's stderr so a full pipe cannot stall it.
fn drain_stderr(stderr: impl io::Read + Send + 'static) {
    thread::spawn(move || {
        let mut sink = io::BufReader::new(stderr);
        let mut scratch = Vec::new();
        let _ = io::Read::read_to_end(&mut sink, &mut scratch);
    });
}

/// Whether a directory looks like a Cargo workspace root.
///
/// The manifest's presence, not its contents: deciding what a
/// workspace *is* belongs to Cargo, and this only needs to know where
/// to point the server.
#[must_use]
pub fn is_cargo_root(directory: &Path) -> bool {
    fs::metadata(directory.join("Cargo.toml")).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_executable_is_reported_rather_than_searched_for_on_path() {
        let error = RustInstall::at("/nowhere/rust-analyzer").expect_err("absent");
        let said = error.to_string();
        assert!(said.contains("/nowhere/rust-analyzer"), "{said}");
        assert!(
            said.contains("never searches PATH"),
            "the reason says what it will not do instead: {said}"
        );
    }

    #[test]
    fn a_version_string_becomes_a_number() {
        assert_eq!(
            trim_version("rust-analyzer 1.98.1 (48a229ce 2026-09-01)"),
            "1.98.1"
        );
        assert_eq!(trim_version("1.98.1"), "1.98.1");
        // Nothing numeric: keep it whole rather than invent one.
        assert_eq!(trim_version("unknown"), "unknown");
    }

    #[test]
    fn a_sysroot_contributes_a_name_and_never_a_home_directory() {
        let install = RustInstall {
            executable: PathBuf::from("/x/rust-analyzer"),
            server_version: "1.98.1".to_owned(),
            rustc_version: String::new(),
            host_triple: String::new(),
            sysroot_identity: String::new(),
            rust_src: false,
        }
        .with_toolchain(
            "rustc 1.98.1",
            "aarch64-apple-darwin",
            Path::new("/Users/someone/.rustup/toolchains/stable-aarch64-apple-darwin"),
        );
        assert_eq!(install.sysroot_identity, "stable-aarch64-apple-darwin");
        assert!(
            !install.sysroot_identity.contains("/Users/"),
            "a home directory is a locator, never identity"
        );
        assert!(!install.rust_src, "that tree does not exist here");
    }

    #[test]
    fn the_command_line_is_the_executable_and_nothing_else() {
        let launcher = RustLauncher::new(
            RustInstall {
                executable: PathBuf::from("/x/rust-analyzer"),
                server_version: "1.98.1".to_owned(),
                rustc_version: String::new(),
                host_triple: String::new(),
                sysroot_identity: String::new(),
                rust_src: false,
            },
            ProjectExecutionTrust::Untrusted,
        );
        assert_eq!(launcher.command_line(), "/x/rust-analyzer");
        assert_eq!(launcher.trust(), ProjectExecutionTrust::Untrusted);
    }

    #[test]
    fn a_launcher_carries_the_trust_decision_rather_than_defaulting_to_yes() {
        let install = RustInstall {
            executable: PathBuf::from("/x/rust-analyzer"),
            server_version: "1.98.1".to_owned(),
            rustc_version: String::new(),
            host_triple: String::new(),
            sysroot_identity: String::new(),
            rust_src: false,
        };
        // There is no `RustLauncher::new(install)`; trust is an
        // argument, so a caller has to have an answer.
        assert!(
            !RustLauncher::new(install.clone(), ProjectExecutionTrust::Untrusted)
                .trust()
                .may_load_projects()
        );
        assert!(
            RustLauncher::new(install, ProjectExecutionTrust::Trusted)
                .trust()
                .may_load_projects()
        );
    }

    #[test]
    fn a_cargo_root_is_recognised_by_its_manifest() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(is_cargo_root(here));
        assert!(!is_cargo_root(&here.join("src")));
    }
}
