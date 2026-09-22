//! Whether a Workspace's own build logic may be executed.
//!
//! Established by #19 task 12 for C# and language-neutral from the
//! start: it is about *executing what the Workspace says to execute*,
//! which every compiled language has a version of. MSBuild evaluates a
//! project and runs its targets; Cargo runs `build.rs` and proc macros.
//! The question is the same, so the answer is one type rather than two.
//!
//! What each backend must state for itself is what its own loading
//! path was measured to execute — see the `MEASURED_*` constants beside
//! each backend's protocol.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Whether this Workspace may have its project build logic executed.
///
/// A type rather than a `bool`, and the reason is the call site: the
/// answer has to be readable everywhere something could start a build,
/// and `false` is not self-describing.
///
/// The default is [`Self::Untrusted`] and nothing infers otherwise. Not
/// that the repository is local, not that it is a Git checkout, not
/// that an Agent is already editing it, not that it built before, not
/// that someone opened the directory. A backend runs in its own
/// process, and that is crash and resource isolation — it is not a
/// security boundary, and treating it as one would be the whole
/// mistake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectExecutionTrust {
    /// No project load, and therefore nothing of the Workspace's own
    /// choosing runs.
    ///
    /// What survives is real and bounded, and what needs a compilation
    /// is an honest gap. Level A is not claimed.
    Untrusted,
    /// An explicit decision, made outside every backend, that this
    /// Workspace's build logic may run.
    Trusted,
}

impl ProjectExecutionTrust {
    /// The only default there is.
    #[must_use]
    pub const fn default_for_workspace() -> Self {
        Self::Untrusted
    }

    /// Whether the project-loading path may be taken.
    #[must_use]
    pub const fn may_load_projects(self) -> bool {
        matches!(self, Self::Trusted)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Untrusted => "UNTRUSTED",
            Self::Trusted => "TRUSTED",
        }
    }
}

impl Default for ProjectExecutionTrust {
    fn default() -> Self {
        Self::default_for_workspace()
    }
}

impl fmt::Display for ProjectExecutionTrust {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::ProjectExecutionTrust;

    #[test]
    fn a_workspace_is_untrusted_until_something_says_otherwise() {
        assert_eq!(
            ProjectExecutionTrust::default(),
            ProjectExecutionTrust::Untrusted
        );
        assert_eq!(
            ProjectExecutionTrust::default_for_workspace(),
            ProjectExecutionTrust::Untrusted
        );
        assert!(!ProjectExecutionTrust::Untrusted.may_load_projects());
        assert!(ProjectExecutionTrust::Trusted.may_load_projects());
    }

    /// The string is persisted inside a configuration basis, so it is a
    /// stored value rather than a label.
    #[test]
    fn the_names_are_stable() {
        assert_eq!(ProjectExecutionTrust::Untrusted.as_str(), "UNTRUSTED");
        assert_eq!(ProjectExecutionTrust::Trusted.as_str(), "TRUSTED");
    }
}
