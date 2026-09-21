//! Daemon runtime state backing the `Status` response (#15 task 9).
//!
//! I1 status only reports what a daemon process actually knows about
//! itself: process/build/protocol identity and how long it has been
//! running. No I2+ concept (Workspace, generation, watcher) is fabricated
//! here -- there is nothing yet that tracks it.

use std::time::{SystemTime, UNIX_EPOCH};

use brainprint_core::{BuildInfo, protocol::StatusResponse};

/// What this daemon process actually knows about itself.
#[derive(Debug, Clone, Copy)]
pub struct DaemonState {
    started_at_unix_ms: u64,
}

impl DaemonState {
    /// Capture the current moment as this daemon's start time.
    #[must_use]
    pub fn new() -> Self {
        Self {
            started_at_unix_ms: unix_millis_now(),
        }
    }

    #[must_use]
    pub fn status(&self) -> StatusResponse {
        let build = BuildInfo::current();
        let now = unix_millis_now();

        StatusResponse {
            daemon_version: build.version.to_owned(),
            protocol_version: build.protocol_version,
            pid: std::process::id(),
            started_at_unix_ms: self.started_at_unix_ms,
            uptime_seconds: now.saturating_sub(self.started_at_unix_ms) / 1000,
        }
    }
}

impl Default for DaemonState {
    fn default() -> Self {
        Self::new()
    }
}

fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reports_this_process_pid_and_current_build_identity() {
        let state = DaemonState::new();
        let status = state.status();

        assert_eq!(status.pid, std::process::id());
        assert_eq!(
            status.protocol_version,
            BuildInfo::current().protocol_version
        );
        assert_eq!(status.daemon_version, BuildInfo::current().version);
    }

    #[test]
    fn uptime_never_goes_negative() {
        let state = DaemonState::new();
        let status = state.status();

        // started_at is captured at/near "now", so uptime should be a
        // small non-negative number of seconds, never underflowing.
        assert!(status.uptime_seconds < 60);
    }
}
