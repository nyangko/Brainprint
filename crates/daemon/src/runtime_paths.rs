//! Daemon IPC endpoint resolution (#15 task 9 / #13 task 4 §3-5).
//!
//! *Where* the endpoint lives is daemon startup policy, not part of the
//! transport contract itself (see
//! [`brainprint_core::protocol::transport`]) -- this module only computes
//! paths, exactly like [`brainprint_engine::paths::WorkspacePaths`] does
//! for Workspace paths; it performs no I/O.

use std::path::{Path, PathBuf};

use brainprint_engine::paths::GlobalPaths;

/// A resolved daemon IPC endpoint plus its singleton lock file path.
#[derive(Debug, Clone)]
pub struct RuntimeEndpoint {
    pub runtime_root: PathBuf,
    #[cfg(unix)]
    pub socket_path: PathBuf,
    #[cfg(windows)]
    pub pipe_name: String,
    /// Singleton daemon-start lock (#13 task 4 §6-7).
    pub lock_path: PathBuf,
}

impl RuntimeEndpoint {
    /// Resolve the endpoint this user's daemon binds/connects to.
    #[must_use]
    pub fn resolve(global_paths: &GlobalPaths) -> Self {
        let runtime_root = global_paths.runtime_root();

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
/// itself -- pipe ACL (task 9 daemon-side bind) is what actually restricts
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_derives_endpoint_paths_from_the_runtime_root() {
        let global_paths = GlobalPaths::from_home("/home/tester");
        let endpoint = RuntimeEndpoint::resolve(&global_paths);

        assert_eq!(endpoint.runtime_root, global_paths.runtime_root());
        assert_eq!(
            endpoint.lock_path,
            global_paths.runtime_root().join("daemon.lock")
        );
        #[cfg(unix)]
        assert_eq!(
            endpoint.socket_path,
            global_paths.runtime_root().join("brainprintd.sock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_too_long_home_path_falls_back_to_a_short_deterministic_socket_path() {
        let long_home = format!("/home/{}", "x".repeat(200));
        let global_paths = GlobalPaths::from_home(&long_home);

        let endpoint = RuntimeEndpoint::resolve(&global_paths);

        assert!(
            endpoint.socket_path.as_os_str().len() <= MAX_UNIX_SOCKET_PATH_BYTES,
            "fallback socket path must fit in sockaddr_un: {:?}",
            endpoint.socket_path
        );
        // Deterministic: the same long home always derives the same short
        // path, so an independently-resolving client finds the same daemon.
        let second_resolution = RuntimeEndpoint::resolve(&global_paths);
        assert_eq!(endpoint.socket_path, second_resolution.socket_path);
    }
}
