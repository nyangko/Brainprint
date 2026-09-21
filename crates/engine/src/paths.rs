//! Physical Brainprint path contracts.

use std::{
    env,
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

#[cfg(windows)]
use std::ffi::OsString;

/// Error returned when Brainprint cannot resolve a required platform path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathResolutionError {
    /// The current process environment does not expose a usable user home directory.
    HomeUnavailable,
}

impl fmt::Display for PathResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HomeUnavailable => {
                formatter.write_str("unable to determine the current user's home directory")
            }
        }
    }
}

impl Error for PathResolutionError {}

/// User-global Brainprint paths rooted at `~/.brainprint`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalPaths {
    pub root: PathBuf,
    pub config_file: PathBuf,
    pub data_dir: PathBuf,
    pub global_db: PathBuf,
    pub logs_dir: PathBuf,
    pub cache_dir: PathBuf,
    /// Portable fallback. Platform-specific runtime selection may choose another location.
    pub fallback_runtime_dir: PathBuf,
}

impl GlobalPaths {
    /// Resolve Brainprint paths from the current user's home directory.
    pub fn discover() -> Result<Self, PathResolutionError> {
        user_home_dir()
            .map(Self::from_home)
            .ok_or(PathResolutionError::HomeUnavailable)
    }

    /// Resolve Brainprint paths below an explicitly supplied home directory.
    #[must_use]
    pub fn from_home(home: impl AsRef<Path>) -> Self {
        let root = home.as_ref().join(".brainprint");

        Self {
            config_file: root.join("config.toml"),
            data_dir: root.join("data"),
            global_db: root.join("data").join("global.db"),
            logs_dir: root.join("logs"),
            cache_dir: root.join("cache"),
            fallback_runtime_dir: root.join("runtime"),
            root,
        }
    }
}

/// Workspace-local Brainprint paths rooted at `<workspace>/.brainprint`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePaths {
    pub workspace_root: PathBuf,
    pub root: PathBuf,
    pub identity_file: PathBuf,
    pub config_file: PathBuf,
    pub data_dir: PathBuf,
    pub project_db: PathBuf,
    pub workspace_db: PathBuf,
    pub index_db: PathBuf,
    pub cache_dir: PathBuf,
}

impl WorkspacePaths {
    /// Resolve paths for an explicitly supplied workspace root.
    #[must_use]
    pub fn from_root(workspace_root: impl AsRef<Path>) -> Self {
        let workspace_root = workspace_root.as_ref().to_path_buf();
        let root = workspace_root.join(".brainprint");
        let data_dir = root.join("data");

        Self {
            identity_file: root.join("workspace.toml"),
            config_file: root.join("config.toml"),
            project_db: data_dir.join("project.db"),
            workspace_db: data_dir.join("workspace.db"),
            index_db: data_dir.join("index.db"),
            cache_dir: root.join("cache"),
            data_dir,
            root,
            workspace_root,
        }
    }
}

#[cfg(not(windows))]
fn user_home_dir() -> Option<PathBuf> {
    non_empty_env("HOME").map(PathBuf::from)
}

#[cfg(windows)]
fn user_home_dir() -> Option<PathBuf> {
    if let Some(profile) = non_empty_env("USERPROFILE") {
        return Some(PathBuf::from(profile));
    }

    let drive = non_empty_env("HOMEDRIVE")?;
    let path = non_empty_env("HOMEPATH")?;
    let mut combined = OsString::from(drive);
    combined.push(path);
    Some(PathBuf::from(combined))
}

fn non_empty_env(name: &str) -> Option<std::ffi::OsString> {
    env::var_os(name).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_paths_follow_physical_contract() {
        let paths = GlobalPaths::from_home("/home/tester");

        assert_eq!(paths.root, PathBuf::from("/home/tester/.brainprint"));
        assert_eq!(
            paths.config_file,
            PathBuf::from("/home/tester/.brainprint/config.toml")
        );
        assert_eq!(
            paths.global_db,
            PathBuf::from("/home/tester/.brainprint/data/global.db")
        );
        assert_eq!(
            paths.fallback_runtime_dir,
            PathBuf::from("/home/tester/.brainprint/runtime")
        );
    }

    #[test]
    fn workspace_paths_follow_physical_contract() {
        let paths = WorkspacePaths::from_root("/work/repo");

        assert_eq!(paths.workspace_root, PathBuf::from("/work/repo"));
        assert_eq!(paths.root, PathBuf::from("/work/repo/.brainprint"));
        assert_eq!(
            paths.identity_file,
            PathBuf::from("/work/repo/.brainprint/workspace.toml")
        );
        assert_eq!(
            paths.project_db,
            PathBuf::from("/work/repo/.brainprint/data/project.db")
        );
        assert_eq!(
            paths.workspace_db,
            PathBuf::from("/work/repo/.brainprint/data/workspace.db")
        );
        assert_eq!(
            paths.index_db,
            PathBuf::from("/work/repo/.brainprint/data/index.db")
        );
    }
}
