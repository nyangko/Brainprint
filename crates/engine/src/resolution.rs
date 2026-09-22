//! The five resolution axes, the resolution context, and the provenance
//! basis a relation is published against (#17 task 2 / #2 relation model).
//!
//! ## Why five, and not one number
//!
//! "How much do we trust this edge" is not one question. A CALLS edge can
//! be resolved to exactly one internal target, dispatched dynamically,
//! extracted from a file the parser only partially understood, and
//! describe a revision that has since moved on -- four independent facts,
//! each actionable in a different way. Collapsing them into a confidence
//! score is how "0.6" ends up meaning nothing to the caller and
//! everything to whoever tuned it. So there are five closed vocabularies
//! and no score anywhere in this crate.
//!
//! - [`Resolution`] -- is the target known, guessed at, or missing.
//! - [`TargetScope`] -- does the target live in this Workspace.
//! - [`Dispatch`] -- is the call site statically bound.
//! - [`Support`] -- did the analysis that produced it actually cover the
//!   source.
//! - [`Freshness`] -- does it still describe what is on disk.
//!
//! ## Where each one lives
//!
//! [`Resolution`] is not stored: a `relation` row *is* a RESOLVED edge
//! (#17 task 2). Candidates and unresolved references have their own
//! tables and their own lifecycle in task 7, and nothing here writes a
//! relation to mean "maybe".
//!
//! [`TargetScope`] is derived from the target endpoint, never taken from
//! the caller -- an external package is external because of what it is.
//! [`Dispatch`] is the one axis an extractor genuinely observes, so it is
//! stored, and "not known" is `UNKNOWN` rather than NULL.
//!
//! [`Support`] and [`Freshness`] are *derived*: the Resource's structural
//! state (#16 task 14), its current revision, and the component state
//! already carry everything they need, and a stored copy would be one
//! more thing that can disagree with the truth. They are computed here by
//! pure functions rather than persisted as columns.

use std::fmt;

use brainprint_core::ResourceId;
use sha2::{Digest, Sha256};

use crate::structural::StructuralState;

/// Whether a target is known.
///
/// Persisted relations are always [`Self::Resolved`]: the other two
/// states describe evidence that has no confirmed target yet, which is
/// `unresolved_reference`/`relation_candidate` territory (#17 task 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Exactly one target, established deterministically.
    Resolved,
    /// One or more possible targets, none of them confirmed. Never
    /// promoted to [`Self::Resolved`] just because there is only one.
    Candidate,
    /// Evidence exists, and no target could be established for it. The
    /// evidence is kept anyway -- dropping it would be a false zero.
    Unresolved,
}

/// Whether the target is inside this Workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetScope {
    Internal,
    External,
}

/// How the call site binds to its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    /// Statically bound: the target is determined by the code as written.
    Static,
    /// Dispatched at runtime -- a virtual/interface call, a callback, a
    /// duck-typed attribute.
    Dynamic,
    /// Not determined. The honest state for an extractor that has not
    /// proven either, and the reason this column is never NULL.
    Unknown,
}

/// How much of the owning Resource the analysis behind an edge covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// Whole-file analysis applies, so an absent edge is meaningful.
    Supported,
    /// The Resource is only partially covered; absence proves nothing.
    Partial,
    /// Nothing covers the Resource. An empty result here is a statement
    /// about capability, not about the code.
    Unsupported,
}

/// Whether an edge still describes what is on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Published against the Resource's current revision, and nothing is
    /// known to have moved since.
    Fresh,
    /// The revision still matches, but a change was observed and the
    /// index has not been recovered yet.
    Dirty,
    /// Published against an older revision of the Resource. Still the
    /// last valid evidence; not current.
    Stale,
}

macro_rules! closed_vocabulary {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $name {
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text,)+
                }
            }

            /// Decode a stored value. An unrecognized one is an error,
            /// never a silent fallback to some default state.
            pub fn parse(raw: &str) -> Result<Self, UnknownAxisValue> {
                match raw {
                    $($text => Ok(Self::$variant),)+
                    other => Err(UnknownAxisValue {
                        axis: stringify!($name),
                        raw: other.to_owned(),
                    }),
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

// Shared with the semantic contract (#19 task 1), which reports
// capability support in the same closed-vocabulary style.
pub(crate) use closed_vocabulary;

closed_vocabulary!(Resolution {
    Resolved => "RESOLVED",
    Candidate => "CANDIDATE",
    Unresolved => "UNRESOLVED",
});

closed_vocabulary!(TargetScope {
    Internal => "INTERNAL",
    External => "EXTERNAL",
});

closed_vocabulary!(Dispatch {
    Static => "STATIC",
    Dynamic => "DYNAMIC",
    Unknown => "UNKNOWN",
});

closed_vocabulary!(Support {
    Supported => "SUPPORTED",
    Partial => "PARTIAL",
    Unsupported => "UNSUPPORTED",
});

closed_vocabulary!(Freshness {
    Fresh => "FRESH",
    Dirty => "DIRTY",
    Stale => "STALE",
});

/// A stored value outside one of the closed vocabularies above.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownAxisValue {
    pub axis: &'static str,
    pub raw: String,
}

impl fmt::Display for UnknownAxisValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown {} value {:?}", self.axis, self.raw)
    }
}

impl std::error::Error for UnknownAxisValue {}

/// The [`Support`] a Resource's structural state implies.
///
/// `None` means nothing has analyzed the Resource yet, which is not the
/// same as analyzing it and finding nothing.
#[must_use]
pub const fn support_of(state: Option<StructuralState>) -> Support {
    match state {
        Some(StructuralState::Complete) => Support::Supported,
        // A file that stopped parsing, or a container whose embedded
        // script is not read, is covered in part. Absence of an edge
        // there proves nothing.
        Some(StructuralState::Partial | StructuralState::ContainerOnly) => Support::Partial,
        Some(
            StructuralState::Unsupported
            | StructuralState::Unavailable
            | StructuralState::GeneratedUnmapped,
        )
        | None => Support::Unsupported,
    }
}

/// The [`Freshness`] of evidence published against `basis_revision`.
///
/// Pure, and deliberately not a column: the Resource row's current
/// revision and the component state already answer this, and a stored
/// copy would be one more thing that can disagree with them.
///
/// `component_current` is whether the structural component covering the
/// owner is currently CURRENT (#16 task 14).
#[must_use]
pub fn freshness_of(
    basis_revision: &str,
    current_resource_revision: &str,
    component_current: bool,
) -> Freshness {
    if basis_revision != current_resource_revision {
        // The Resource moved on. The evidence is the last valid one, and
        // saying so is the whole point of keeping it.
        return Freshness::Stale;
    }
    if component_current {
        Freshness::Fresh
    } else {
        Freshness::Dirty
    }
}

/// The weaker of two [`Support`] values.
///
/// One definition, shared by every aggregate that has to answer "how
/// well covered is all of this" (#17 tasks 8 and 11): the weakest part
/// decides, because a whole is not better covered than its worst piece.
#[must_use]
pub(crate) const fn weaker_support(left: Support, right: Support) -> Support {
    match (left, right) {
        (Support::Unsupported, _) | (_, Support::Unsupported) => Support::Unsupported,
        (Support::Partial, _) | (_, Support::Partial) => Support::Partial,
        _ => Support::Supported,
    }
}

/// The weaker of two [`Freshness`] values, on the same principle.
#[must_use]
pub(crate) const fn weaker_freshness(left: Freshness, right: Freshness) -> Freshness {
    match (left, right) {
        (Freshness::Stale, _) | (_, Freshness::Stale) => Freshness::Stale,
        (Freshness::Dirty, _) | (_, Freshness::Dirty) => Freshness::Dirty,
        _ => Freshness::Fresh,
    }
}

/// What a resolution depended on, beyond the source file itself.
///
/// Two Workspaces -- or one Workspace before and after a dependency
/// bump -- can read the same import statement and resolve it to different
/// things. The context is the fingerprint of everything outside the file
/// that decided the answer, so a resolution can be invalidated when its
/// inputs move without re-reading the file.
///
/// Fingerprints are the caller's: this type stores hashes and identity,
/// never raw config, lockfile, or source text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionContext {
    pub language: String,
    /// What the context covers -- a package root, a project file, a
    /// compilation unit. Deterministic, and the caller's vocabulary.
    pub scope_key: String,
    pub config_fingerprint: String,
    pub dependency_fingerprint: String,
    pub environment_fingerprint: String,
    pub module_resolution_fingerprint: String,
    /// A semantic backend's own snapshot token, when one is involved
    /// (I4). `None` for a purely structural resolution.
    pub backend_snapshot_token: Option<String>,
}

/// Tag on every generated `resolution_context.context_key`, so a stored
/// key is self-describing if the derivation ever changes.
pub const CONTEXT_KEY_ALGORITHM: &str = "sha256-rc1";

impl ResolutionContext {
    /// The deterministic identity of this context.
    ///
    /// Same inputs, same key, on any machine and in any order of
    /// discovery -- which is what makes "the same context" reuse one row
    /// instead of accumulating near-duplicates. Field boundaries are
    /// length-prefixed so that moving a character across a boundary
    /// cannot produce the same digest.
    #[must_use]
    pub fn context_key(&self) -> String {
        let mut hasher = Sha256::new();
        for field in [
            Some(self.language.as_str()),
            Some(self.scope_key.as_str()),
            Some(self.config_fingerprint.as_str()),
            Some(self.dependency_fingerprint.as_str()),
            Some(self.environment_fingerprint.as_str()),
            Some(self.module_resolution_fingerprint.as_str()),
            self.backend_snapshot_token.as_deref(),
        ] {
            match field {
                Some(value) => {
                    hasher.update(b"s");
                    hasher.update(value.len().to_le_bytes());
                    hasher.update(value.as_bytes());
                }
                None => hasher.update(b"n"),
            }
        }
        format!("{CONTEXT_KEY_ALGORITHM}:{:x}", hasher.finalize())
    }
}

/// What a piece of relation evidence was produced from.
///
/// The runtime contract only: attaching it to actual Occurrence and
/// Relation rows in one atomic replacement is #17 task 3. Keeping it a
/// plain value means the extractor (task 4+) and the storage boundary
/// agree on what provenance *is* before either of them writes one.
///
/// The extractor and adapter semantics versions are not repeated here:
/// they are part of the `analysis_profile` this basis names, and copying
/// them would create a second place for them to drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceBasis {
    /// The Resource whose re-analysis owns -- and may replace -- this
    /// evidence.
    pub owner_resource: ResourceId,
    /// The owner's `resource_revision` the analysis read.
    pub owner_resource_revision: String,
    /// The generation publishing it.
    pub generation_id: i64,
    /// The profile that produced it, which carries the backend and
    /// semantics versions.
    pub analysis_profile_id: i64,
    /// The context the target resolution depended on, when it depended on
    /// anything outside the file. `None` for a resolution that needed
    /// only the source itself.
    pub resolution_context_key: Option<String>,
}

impl EvidenceBasis {
    /// Whether this basis still describes the Resource as it is now.
    #[must_use]
    pub fn freshness(&self, current_resource_revision: &str, component_current: bool) -> Freshness {
        freshness_of(
            &self.owner_resource_revision,
            current_resource_revision,
            component_current,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_axis_round_trips_and_refuses_anything_else() {
        for value in [
            Resolution::Resolved,
            Resolution::Candidate,
            Resolution::Unresolved,
        ] {
            assert_eq!(Resolution::parse(value.as_str()), Ok(value));
        }
        for value in [TargetScope::Internal, TargetScope::External] {
            assert_eq!(TargetScope::parse(value.as_str()), Ok(value));
        }
        for value in [Dispatch::Static, Dispatch::Dynamic, Dispatch::Unknown] {
            assert_eq!(Dispatch::parse(value.as_str()), Ok(value));
        }
        for value in [Support::Supported, Support::Partial, Support::Unsupported] {
            assert_eq!(Support::parse(value.as_str()), Ok(value));
        }
        for value in [Freshness::Fresh, Freshness::Dirty, Freshness::Stale] {
            assert_eq!(Freshness::parse(value.as_str()), Ok(value));
        }

        // Neighbouring vocabularies do not leak into one another, and
        // nothing decodes to a default.
        assert!(Resolution::parse("INTERNAL").is_err());
        assert!(TargetScope::parse("UNKNOWN").is_err());
        assert!(Dispatch::parse("").is_err());
        assert!(Support::parse("supported").is_err());
        assert!(Freshness::parse("CURRENT").is_err());
        assert_eq!(
            Dispatch::parse("MAYBE").expect_err("refused").axis,
            "Dispatch"
        );
    }

    #[test]
    fn support_follows_the_resources_structural_coverage() {
        assert_eq!(
            support_of(Some(StructuralState::Complete)),
            Support::Supported
        );
        for partial in [StructuralState::Partial, StructuralState::ContainerOnly] {
            assert_eq!(support_of(Some(partial)), Support::Partial);
        }
        for unsupported in [
            StructuralState::Unsupported,
            StructuralState::Unavailable,
            StructuralState::GeneratedUnmapped,
        ] {
            assert_eq!(support_of(Some(unsupported)), Support::Unsupported);
        }
        assert_eq!(
            support_of(None),
            Support::Unsupported,
            "never analyzed is not the same as analyzed and empty"
        );
    }

    #[test]
    fn freshness_separates_a_moved_revision_from_an_unrecovered_index() {
        assert_eq!(freshness_of("3", "3", true), Freshness::Fresh);
        assert_eq!(
            freshness_of("3", "3", false),
            Freshness::Dirty,
            "same revision, something seen but not yet recovered"
        );
        assert_eq!(
            freshness_of("2", "3", true),
            Freshness::Stale,
            "the Resource moved on: last-valid, not current"
        );
        assert_eq!(freshness_of("2", "3", false), Freshness::Stale);
    }

    fn context() -> ResolutionContext {
        ResolutionContext {
            language: "TYPESCRIPT".to_owned(),
            scope_key: "tsconfig.json".to_owned(),
            config_fingerprint: "sha256:config".to_owned(),
            dependency_fingerprint: "sha256:deps".to_owned(),
            environment_fingerprint: "sha256:env".to_owned(),
            module_resolution_fingerprint: "sha256:module".to_owned(),
            backend_snapshot_token: None,
        }
    }

    #[test]
    fn a_context_key_is_deterministic_and_field_boundaries_are_not_ambiguous() {
        assert_eq!(context().context_key(), context().context_key());
        assert!(context().context_key().starts_with("sha256-rc1:"));

        let mut moved = context();
        moved.language = "TYPESCRIP".to_owned();
        moved.scope_key = "Ttsconfig.json".to_owned();
        assert_ne!(
            moved.context_key(),
            context().context_key(),
            "a character moved across a field boundary is a different context"
        );

        let mut token = context();
        token.backend_snapshot_token = Some(String::new());
        assert_ne!(
            token.context_key(),
            context().context_key(),
            "an empty token is not the absence of a token"
        );

        for mutate in [
            |context: &mut ResolutionContext| context.config_fingerprint = "other".to_owned(),
            |context: &mut ResolutionContext| context.dependency_fingerprint = "other".to_owned(),
            |context: &mut ResolutionContext| context.environment_fingerprint = "other".to_owned(),
            |context: &mut ResolutionContext| {
                context.module_resolution_fingerprint = "other".to_owned();
            },
        ] {
            let mut changed = context();
            mutate(&mut changed);
            assert_ne!(changed.context_key(), context().context_key());
        }
    }

    #[test]
    fn an_evidence_basis_answers_freshness_without_storing_it() {
        let basis = EvidenceBasis {
            owner_resource: brainprint_core::ResourceId::generate(),
            owner_resource_revision: "4".to_owned(),
            generation_id: 7,
            analysis_profile_id: 1,
            resolution_context_key: Some(context().context_key()),
        };
        assert_eq!(basis.freshness("4", true), Freshness::Fresh);
        assert_eq!(basis.freshness("4", false), Freshness::Dirty);
        assert_eq!(basis.freshness("5", true), Freshness::Stale);
    }
}
