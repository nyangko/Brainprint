//! User-editable Brainprint configuration bootstrap.

use std::{
    error::Error,
    fmt,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    paths::{GlobalPaths, WorkspacePaths},
    trust::ProjectExecutionTrust,
};

/// Current on-disk TOML configuration format.
pub const CONFIG_FORMAT_VERSION: u32 = 1;

/// Minimal user-global configuration.
///
/// Additional settings can be added as I1/I5 needs them without changing path ownership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
    pub format_version: u32,
    /// Where the user installed each semantic backend (#39). Absent
    /// means UNAVAILABLE; nothing is ever downloaded or searched for.
    #[serde(default, skip_serializing_if = "SemanticBackendLocators::is_empty")]
    pub semantic_backends: SemanticBackendLocators,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            format_version: CONFIG_FORMAT_VERSION,
            semantic_backends: SemanticBackendLocators::default(),
        }
    }
}

/// Explicit semantic backend install locators, one optional entry per
/// backend family (#39 "Backend locator contract"). A Workspace entry
/// overrides the global entry of the same family.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticBackendLocators {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<NodeBackendLocator>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typescript: Option<InstallRootLocator>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub svelte: Option<NodeBackendLocator>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub csharp: Option<InstallRootLocator>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rust: Option<ExecutableLocator>,
}

impl SemanticBackendLocators {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// An install root for a backend the existing launcher runs under Node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeBackendLocator {
    pub install_root: PathBuf,
    /// Omitted: the launcher's existing default executable name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRootLocator {
    pub install_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableLocator {
    pub executable: PathBuf,
}

/// Minimal workspace-local configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub format_version: u32,
    /// Additional directory names Resource discovery (#16 task 2) should
    /// exclude, on top of its deterministic default list. Names only --
    /// not path globs -- matching the same deterministic, non-glob rule
    /// the default exclusion list uses.
    #[serde(default)]
    pub extra_excluded_directory_names: Vec<String>,
    /// Per-Workspace backend locator overrides (#39).
    #[serde(default, skip_serializing_if = "SemanticBackendLocators::is_empty")]
    pub semantic_backends: SemanticBackendLocators,
    /// The user's explicit per-Workspace decision that its project build
    /// logic may run (#39). Absent means Untrusted; there is deliberately
    /// no global equivalent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_execution_trust: Option<ProjectExecutionTrust>,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            format_version: CONFIG_FORMAT_VERSION,
            extra_excluded_directory_names: Vec::new(),
            semantic_backends: SemanticBackendLocators::default(),
            project_execution_trust: None,
        }
    }
}

/// Configuration load/bootstrap failure.
#[derive(Debug)]
pub enum ConfigError {
    MissingParent {
        path: PathBuf,
    },
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Decode {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    Encode {
        source: Box<toml::ser::Error>,
    },
    UnsupportedFormat {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingParent { path } => {
                write!(
                    formatter,
                    "configuration path has no parent: {}",
                    path.display()
                )
            }
            Self::Io { path, source } => {
                write!(
                    formatter,
                    "configuration I/O failed at {}: {source}",
                    path.display()
                )
            }
            Self::Decode { path, source } => {
                write!(
                    formatter,
                    "invalid configuration at {}: {source}",
                    path.display()
                )
            }
            Self::Encode { source } => {
                write!(formatter, "failed to encode configuration: {source}")
            }
            Self::UnsupportedFormat {
                path,
                found,
                supported,
            } => write!(
                formatter,
                "unsupported configuration format at {}: found {found}, supported {supported}",
                path.display()
            ),
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Decode { source, .. } => Some(source.as_ref()),
            Self::Encode { source } => Some(source.as_ref()),
            Self::MissingParent { .. } | Self::UnsupportedFormat { .. } => None,
        }
    }
}

/// Load an existing global config or create the minimal default if absent.
pub fn bootstrap_global_config(paths: &GlobalPaths) -> Result<GlobalConfig, ConfigError> {
    bootstrap_config(&paths.config_file)
}

/// Load an existing workspace config or create the minimal default if absent.
pub fn bootstrap_workspace_config(paths: &WorkspacePaths) -> Result<WorkspaceConfig, ConfigError> {
    bootstrap_config(&paths.config_file)
}

/// Load and validate an existing global configuration.
pub fn load_global_config(paths: &GlobalPaths) -> Result<GlobalConfig, ConfigError> {
    load_config(&paths.config_file)
}

/// Load and validate an existing workspace configuration.
pub fn load_workspace_config(paths: &WorkspacePaths) -> Result<WorkspaceConfig, ConfigError> {
    load_config(&paths.config_file)
}

trait VersionedConfig {
    fn format_version(&self) -> u32;
}

impl VersionedConfig for GlobalConfig {
    fn format_version(&self) -> u32 {
        self.format_version
    }
}

impl VersionedConfig for WorkspaceConfig {
    fn format_version(&self) -> u32 {
        self.format_version
    }
}

fn bootstrap_config<T>(path: &Path) -> Result<T, ConfigError>
where
    T: Default + Serialize + DeserializeOwned + VersionedConfig,
{
    if path.exists() {
        return load_config(path);
    }

    let config = T::default();
    validate_format(path, &config)?;

    let mut encoded = toml::to_string_pretty(&config).map_err(|source| ConfigError::Encode {
        source: Box::new(source),
    })?;
    if !encoded.ends_with('\n') {
        encoded.push('\n');
    }

    let parent = path.parent().ok_or_else(|| ConfigError::MissingParent {
        path: path.to_path_buf(),
    })?;
    fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            if let Err(source) = file
                .write_all(encoded.as_bytes())
                .and_then(|()| file.sync_all())
            {
                drop(file);
                let _ = fs::remove_file(path);
                return Err(ConfigError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
            Ok(config)
        }
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => load_config(path),
        Err(source) => Err(ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn load_config<T>(path: &Path) -> Result<T, ConfigError>
where
    T: DeserializeOwned + VersionedConfig,
{
    let encoded = fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let config = toml::from_str::<T>(&encoded).map_err(|source| ConfigError::Decode {
        path: path.to_path_buf(),
        source: Box::new(source),
    })?;
    validate_format(path, &config)?;
    Ok(config)
}

fn validate_format<T>(path: &Path, config: &T) -> Result<(), ConfigError>
where
    T: VersionedConfig,
{
    let found = config.format_version();
    if found != CONFIG_FORMAT_VERSION {
        return Err(ConfigError::UnsupportedFormat {
            path: path.to_path_buf(),
            found,
            supported: CONFIG_FORMAT_VERSION,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        env, process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path =
                env::temp_dir().join(format!("brainprint-{label}-{}-{sequence}", process::id()));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn global_bootstrap_creates_only_config_root() {
        let home = TestDir::create("global-config");
        let paths = GlobalPaths::from_home(home.path());

        let config = bootstrap_global_config(&paths).expect("global config should bootstrap");

        assert_eq!(config, GlobalConfig::default());
        assert!(paths.root.is_dir());
        assert!(paths.config_file.is_file());
        assert!(!paths.data_dir.exists());
        assert!(!paths.logs_dir.exists());
        assert!(!paths.cache_dir.exists());
        assert!(!paths.fallback_runtime_dir.exists());
    }

    #[test]
    fn workspace_bootstrap_does_not_precreate_identity_or_data() {
        let workspace = TestDir::create("workspace-config");
        let paths = WorkspacePaths::from_root(workspace.path());

        let config = bootstrap_workspace_config(&paths).expect("workspace config should bootstrap");

        assert_eq!(config, WorkspaceConfig::default());
        assert!(paths.root.is_dir());
        assert!(paths.config_file.is_file());
        assert!(!paths.identity_file.exists());
        assert!(!paths.data_dir.exists());
        assert!(!paths.cache_dir.exists());
    }

    #[test]
    fn bootstrap_preserves_existing_user_config() {
        let home = TestDir::create("existing-config");
        let paths = GlobalPaths::from_home(home.path());
        fs::create_dir_all(&paths.root).expect("config root should be created");

        let original = "# user-owned comment\nformat_version = 1\n";
        fs::write(&paths.config_file, original).expect("fixture config should be written");

        let config = bootstrap_global_config(&paths).expect("existing config should load");

        assert_eq!(config, GlobalConfig::default());
        assert_eq!(
            fs::read_to_string(&paths.config_file).expect("config should remain readable"),
            original
        );
    }

    #[test]
    fn unsupported_format_is_rejected_without_rewrite() {
        let workspace = TestDir::create("unsupported-config");
        let paths = WorkspacePaths::from_root(workspace.path());
        fs::create_dir_all(&paths.root).expect("config root should be created");

        let original = "format_version = 99\n";
        fs::write(&paths.config_file, original).expect("fixture config should be written");

        let error = bootstrap_workspace_config(&paths).expect_err("future config must be rejected");

        assert!(matches!(
            error,
            ConfigError::UnsupportedFormat {
                found: 99,
                supported: CONFIG_FORMAT_VERSION,
                ..
            }
        ));
        assert_eq!(
            fs::read_to_string(&paths.config_file).expect("config should remain readable"),
            original
        );
    }

    #[test]
    fn malformed_config_is_rejected_as_decode_error() {
        let workspace = TestDir::create("malformed-config");
        let paths = WorkspacePaths::from_root(workspace.path());
        fs::create_dir_all(&paths.root).expect("config root should be created");

        fs::write(&paths.config_file, "not [ valid toml")
            .expect("fixture config should be written");

        let error =
            bootstrap_workspace_config(&paths).expect_err("malformed config must be rejected");

        assert!(matches!(error, ConfigError::Decode { .. }));
    }

    #[test]
    fn workspace_config_without_ignore_override_defaults_to_empty() {
        // A config.toml written before #16 task 2 added this field must
        // still load, defaulting to no extra exclusions.
        let workspace = TestDir::create("legacy-workspace-config");
        let paths = WorkspacePaths::from_root(workspace.path());
        fs::create_dir_all(&paths.root).expect("config root should be created");
        fs::write(&paths.config_file, "format_version = 1\n")
            .expect("fixture config should be written");

        let config = load_workspace_config(&paths).expect("legacy config should load");

        assert!(config.extra_excluded_directory_names.is_empty());
    }

    #[test]
    fn workspace_config_ignore_override_round_trips() {
        let workspace = TestDir::create("ignore-override-config");
        let paths = WorkspacePaths::from_root(workspace.path());
        fs::create_dir_all(&paths.root).expect("config root should be created");
        fs::write(
            &paths.config_file,
            "format_version = 1\nextra_excluded_directory_names = [\"vendor\"]\n",
        )
        .expect("fixture config should be written");

        let config = load_workspace_config(&paths).expect("config should load");

        assert_eq!(config.extra_excluded_directory_names, vec!["vendor"]);
    }

    #[test]
    fn missing_config_file_is_reported_as_io_error() {
        let workspace = TestDir::create("missing-config");
        let paths = WorkspacePaths::from_root(workspace.path());

        let error = load_workspace_config(&paths).expect_err("missing config must be reported");

        assert!(matches!(error, ConfigError::Io { .. }));
    }

    #[test]
    fn backend_locators_are_optional_and_strict() {
        let home = TestDir::create("backend-locators");
        let paths = GlobalPaths::from_home(home.path());

        // The bootstrapped default is byte-for-byte what it was before #39.
        bootstrap_global_config(&paths).expect("bootstrap");
        assert_eq!(
            fs::read_to_string(&paths.config_file).expect("config"),
            "format_version = 1\n"
        );

        fs::write(
            &paths.config_file,
            "format_version = 1\n\n[semantic_backends.python]\ninstall_root = \"/opt/pyright\"\nnode = \"/opt/node\"\n\n[semantic_backends.rust]\nexecutable = \"/opt/ra\"\n",
        )
        .expect("write");
        let config = load_global_config(&paths).expect("locators load");
        let python = config.semantic_backends.python.expect("python locator");
        assert_eq!(python.install_root, PathBuf::from("/opt/pyright"));
        assert_eq!(python.node.as_deref(), Some("/opt/node"));
        assert_eq!(
            config.semantic_backends.rust.expect("rust").executable,
            PathBuf::from("/opt/ra")
        );
        assert!(config.semantic_backends.typescript.is_none());

        fs::write(
            &paths.config_file,
            "format_version = 1\n\n[semantic_backends.python]\ninstall_root = \"/x\"\nsurprise = 1\n",
        )
        .expect("write");
        assert!(matches!(
            load_global_config(&paths),
            Err(ConfigError::Decode { .. })
        ));
    }

    #[test]
    fn project_execution_trust_is_an_explicit_workspace_only_opt_in() {
        let workspace = TestDir::create("trust-config");
        let paths = WorkspacePaths::from_root(workspace.path());
        bootstrap_workspace_config(&paths).expect("bootstrap");
        assert_eq!(
            fs::read_to_string(&paths.config_file).expect("config"),
            "format_version = 1\nextra_excluded_directory_names = []\n",
            "the bootstrapped default is unchanged"
        );
        let absent = load_workspace_config(&paths).expect("load");
        assert_eq!(absent.project_execution_trust, None);

        for (value, expected) in [
            ("Trusted", ProjectExecutionTrust::Trusted),
            ("Untrusted", ProjectExecutionTrust::Untrusted),
        ] {
            fs::write(
                &paths.config_file,
                format!("format_version = 1\nproject_execution_trust = \"{value}\"\n"),
            )
            .expect("write");
            assert_eq!(
                load_workspace_config(&paths)
                    .expect("load")
                    .project_execution_trust,
                Some(expected)
            );
        }
        fs::write(
            &paths.config_file,
            "format_version = 1\nproject_execution_trust = \"yes\"\n",
        )
        .expect("write");
        assert!(matches!(
            load_workspace_config(&paths),
            Err(ConfigError::Decode { .. })
        ));

        // No global default can make every Workspace trusted.
        let home = TestDir::create("trust-global");
        let global = GlobalPaths::from_home(home.path());
        fs::create_dir_all(&global.root).expect("root");
        fs::write(
            &global.config_file,
            "format_version = 1\nproject_execution_trust = \"Trusted\"\n",
        )
        .expect("write");
        assert!(matches!(
            load_global_config(&global),
            Err(ConfigError::Decode { .. })
        ));
    }
}
