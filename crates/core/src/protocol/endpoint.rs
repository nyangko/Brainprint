//! Canonical daemon IPC endpoint path derivation (#15 task 9/10 / #13 task
//! 4 §3-5): the single implementation of "given a runtime root, where
//! exactly is the socket/pipe/lock" so `brainprint-daemon` (binds it) and
//! `brainprint-cli` (connects to it) always agree byte-for-byte. This
//! cannot live in `brainprint-engine` or `brainprint-daemon` --
//! `brainprint-cli` must reach it without depending on either (#15 task
//! 10's dependency rule: `brainprint-cli -> brainprint-core` only).
//!
//! Where the *runtime root itself* comes from can still differ by caller:
//! `brainprint-daemon` derives it from
//! [`brainprint_engine::paths::GlobalPaths::runtime_root`] (unchanged from
//! #15 task 9); `brainprint-cli`, which cannot depend on
//! `brainprint-engine`, uses [`EndpointPaths::resolve`]'s own small,
//! self-contained copy of the same `$XDG_RUNTIME_DIR`-preferred rule
//! (#13 task 4 §3-4) -- duplicated rather than shared across that
//! boundary, same tradeoff as the FNV-1a hash below.

use std::path::{Path, PathBuf};

/// A resolved daemon IPC endpoint plus its singleton lock file path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointPaths {
    pub runtime_root: PathBuf,
    #[cfg(unix)]
    pub socket_path: PathBuf,
    #[cfg(windows)]
    pub pipe_name: String,
    /// Singleton daemon-start lock (#13 task 4 §6-7).
    pub lock_path: PathBuf,
}

/// The runtime root could not be resolved: no `$XDG_RUNTIME_DIR` override
/// and no usable home directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointResolutionError;

impl std::fmt::Display for EndpointResolutionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "unable to resolve the daemon runtime endpoint: no $XDG_RUNTIME_DIR and no usable home directory",
        )
    }
}

impl std::error::Error for EndpointResolutionError {}

impl EndpointPaths {
    /// Resolve the endpoint from the real process environment. Used
    /// directly by `brainprint-cli`; `brainprint-daemon` instead calls
    /// [`Self::from_runtime_root`] with the root
    /// `brainprint-engine::paths::GlobalPaths::runtime_root` already
    /// computed, so both still agree on the derivation below.
    pub fn resolve() -> Result<Self, EndpointResolutionError> {
        let runtime_root = resolve_runtime_root().ok_or(EndpointResolutionError)?;
        Ok(Self::from_runtime_root(runtime_root))
    }

    /// Derive endpoint paths from an already-known runtime root.
    #[must_use]
    pub fn from_runtime_root(runtime_root: PathBuf) -> Self {
        Self {
            #[cfg(unix)]
            socket_path: unix_socket_path(&runtime_root),
            #[cfg(windows)]
            pipe_name: windows_pipe_name(),
            lock_path: runtime_root.join("daemon.lock"),
            runtime_root,
        }
    }
}

fn resolve_runtime_root() -> Option<PathBuf> {
    if let Some(xdg_runtime_dir) = non_empty_env("XDG_RUNTIME_DIR") {
        return Some(PathBuf::from(xdg_runtime_dir).join("brainprint"));
    }
    user_home_dir().map(|home| home.join(".brainprint").join("runtime"))
}

/// `AF_UNIX` socket addresses are limited to a small, platform-specific
/// byte count (`sizeof(sun_path)` -- 104 on macOS/BSD, 108 on Linux).
/// Conservative well under either.
#[cfg(unix)]
const MAX_UNIX_SOCKET_PATH_BYTES: usize = 90;

/// `<runtime_root>/brainprintd.sock`, unless that path would not fit in a
/// `sockaddr_un` (#13 task 4 §4: real on a long `$HOME`, especially on
/// macOS without `$XDG_RUNTIME_DIR`) -- in which case this falls back to a
/// short, deterministic path derived from the real one, under a
/// user-private directory in the OS temp root, rather than silently
/// failing every bind/connect with `EINVAL`.
#[cfg(unix)]
fn unix_socket_path(runtime_root: &Path) -> PathBuf {
    let candidate = runtime_root.join("brainprintd.sock");
    if candidate.as_os_str().len() <= MAX_UNIX_SOCKET_PATH_BYTES {
        return candidate;
    }

    std::env::temp_dir()
        .join(format!(
            "bp-{:016x}",
            fnv1a_hash(&runtime_root.to_string_lossy())
        ))
        .join("d.sock")
}

/// `\\.\pipe\brainprint-<user-identity-hash>` (#13 task 4 §5): scoped to
/// the current interactive user so daemons for different users never
/// collide on the machine-wide pipe namespace. Not a security boundary by
/// itself -- pipe ACL (daemon-side bind) is what actually restricts
/// access.
#[cfg(windows)]
fn windows_pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown-user".to_owned());
    format!(r"\\.\pipe\brainprint-{:016x}", fnv1a_hash(&user))
}

/// Dependency-free change/identity hash, deliberately duplicated from the
/// same tiny FNV-1a routine in `brainprint-engine`'s migration checksum
/// (`crates/engine/src/db/migration.rs`) rather than shared across the
/// crate boundary for a one-off, non-cryptographic use.
#[cfg_attr(windows, allow(dead_code))]
fn fnv1a_hash(text: &str) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
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
    let mut combined = std::ffi::OsString::from(drive);
    combined.push(path);
    Some(PathBuf::from(combined))
}

fn non_empty_env(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_runtime_root_derives_endpoint_paths() {
        let runtime_root = PathBuf::from("/home/tester/.brainprint/runtime");
        let endpoint = EndpointPaths::from_runtime_root(runtime_root.clone());

        assert_eq!(endpoint.runtime_root, runtime_root);
        assert_eq!(endpoint.lock_path, runtime_root.join("daemon.lock"));
        #[cfg(unix)]
        assert_eq!(endpoint.socket_path, runtime_root.join("brainprintd.sock"));
    }

    #[cfg(unix)]
    #[test]
    fn a_too_long_runtime_root_falls_back_to_a_short_deterministic_socket_path() {
        let long_root = PathBuf::from(format!("/home/{}/.brainprint/runtime", "x".repeat(200)));

        let endpoint = EndpointPaths::from_runtime_root(long_root.clone());

        assert!(
            endpoint.socket_path.as_os_str().len() <= MAX_UNIX_SOCKET_PATH_BYTES,
            "fallback socket path must fit in sockaddr_un: {:?}",
            endpoint.socket_path
        );
        // Deterministic: the same long root always derives the same short
        // path, so an independently-resolving client finds the same daemon.
        let second = EndpointPaths::from_runtime_root(long_root);
        assert_eq!(endpoint.socket_path, second.socket_path);
    }
}
