//! The lifecycle vocabulary every semantic backend shares.
//!
//! Small on purpose. What lives here is the language-neutral answer to
//! "what can a reader expect of this owner's semantics right now" --
//! the Level B projection #19 tasks 8 and 10 both need, stated once so
//! Python and TypeScript/JavaScript cannot drift into two spellings of
//! the same five states.
//!
//! What deliberately does *not* live here is anything a language
//! decides for itself: project configuration discovery, environment
//! identity, what a change means for module resolution. Those differ in
//! kind, not in wording, and a shared abstraction over them would be an
//! abstraction over a disagreement.

use crate::{
    resolution::Support,
    runtime::RuntimeState,
    semantic_index::{SemanticState, SemanticStatus},
};

/// What a reader can expect of one owner's semantics right now.
///
/// One projection, not a second confidence score: it combines the
/// persisted publication state with the runtime's, because "current
/// but the process is cold" and "the process is up but the answer is
/// stale" are different situations and only one of them needs work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticAvailability {
    /// Published, current, and the runtime is up.
    Current,
    /// Published and current; the backend is not running and does not
    /// need to. Task 3 already separated these axes -- a dead process
    /// does not invalidate a proof whose inputs still hold.
    CurrentRuntimeCold,
    /// Published and current, with the backend covering only part of
    /// the Resource.
    Partial,
    /// A refresh is owed: the basis moved, or nothing was ever
    /// published.
    RefreshRequired,
    /// No semantic answer is obtainable at all -- no install, an
    /// incompatible protocol, or a spent restart budget.
    Unavailable,
}

impl SemanticAvailability {
    /// Whether a semantic result may be served as current.
    #[must_use]
    pub const fn serves_current(self) -> bool {
        matches!(
            self,
            Self::Current | Self::CurrentRuntimeCold | Self::Partial
        )
    }
}

/// Whether a backend could be used at all, independent of whether one
/// is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendReadiness {
    /// An install exists and its protocol is one the adapter reads.
    Available,
    /// No install, an unreadable one, or a protocol outside the tested
    /// compatibility class.
    Unavailable,
}

/// Project one owner's availability from what is persisted and what
/// the runtime is doing.
#[must_use]
pub fn availability(
    status: &SemanticStatus,
    runtime: RuntimeState,
    backend: BackendReadiness,
) -> SemanticAvailability {
    match backend {
        BackendReadiness::Unavailable => {
            if status.state == SemanticState::Current {
                // A proof whose inputs still hold is still a proof. The
                // process being gone is a reason it cannot be *renewed*,
                // not a reason to disbelieve it.
                return SemanticAvailability::CurrentRuntimeCold;
            }
            SemanticAvailability::Unavailable
        }
        BackendReadiness::Available => match status.state {
            SemanticState::Current if status.support == Some(Support::Partial) => {
                SemanticAvailability::Partial
            }
            SemanticState::Current => match runtime {
                RuntimeState::Ready | RuntimeState::Busy | RuntimeState::Idle => {
                    SemanticAvailability::Current
                }
                _ => SemanticAvailability::CurrentRuntimeCold,
            },
            SemanticState::Unavailable => SemanticAvailability::Unavailable,
            _ => SemanticAvailability::RefreshRequired,
        },
    }
}
