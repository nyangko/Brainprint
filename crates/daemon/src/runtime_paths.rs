//! Daemon IPC endpoint resolution (#15 task 9 / #13 task 4 §3-5).
//!
//! *Where* the endpoint lives is daemon startup policy: this resolves
//! [`brainprint_engine::paths::GlobalPaths::runtime_root`] (task 2/9) and
//! hands it to [`brainprint_core::protocol::endpoint::EndpointPaths`] for
//! the actual socket/pipe/lock derivation -- the *same* derivation
//! `brainprint-cli` uses (#15 task 10), so daemon and CLI always agree on
//! where the endpoint is. This module performs no I/O.

use brainprint_core::protocol::endpoint::EndpointPaths;
use brainprint_engine::paths::GlobalPaths;

pub use brainprint_core::protocol::endpoint::EndpointPaths as RuntimeEndpoint;

/// Resolve the endpoint this user's daemon binds/connects to.
#[must_use]
pub fn resolve(global_paths: &GlobalPaths) -> RuntimeEndpoint {
    EndpointPaths::from_runtime_root(global_paths.runtime_root())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_derives_endpoint_paths_from_the_runtime_root() {
        let global_paths = GlobalPaths::from_home("/home/tester");
        let endpoint = resolve(&global_paths);

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
}
