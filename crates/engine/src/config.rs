//! User-editable Brainprint configuration bootstrap.

use std::{
    error::Error,
    fmt,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::paths::{GlobalPaths, WorkspacePaths};

/// Current on-disk TOML configuration format.
pub const CONFIG_FORMAT_VERSION: u32 = 1;

/// Minimal user-global configuration.
///
/// Additional settings can be added as I1/I5 needs them without changing path ownership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
    pub format_version: u32,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            format_version: CONFIG_FORMAT_VERSION,
        }
    }
}

/// Minimal workspace-local configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub format_version: u32,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            format_version: CONFIG_FORMAT_VERSION,
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
    fn missing_config_file_is_reported_as_io_error() {
        let workspace = TestDir::create("missing-config");
        let paths = WorkspacePaths::from_root(workspace.path());

        let error = load_workspace_config(&paths).expect_err("missing config must be reported");

        assert!(matches!(error, ConfigError::Io { .. }));
    }
}
